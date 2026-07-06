# File conversions

How the gateway reads uploaded files and produces documents in other formats,
which tool/engine does each job, and — importantly — what each path can and
**cannot** do. Written for operators and for reasoning about feature gaps.

## TL;DR

- **Reading uploads:** text-ish files and PDFs are read natively; Office files
  (`.docx`/`.pptx`/`.xlsx`) are **not** read by `fetch_attachment` — they must
  go through `convert_document` first.
- **Producing documents:** two families —
  1. **Markdown → PDF/DOCX/PPTX** via `generate_document` (pandoc). Fast, generic,
     unbranded.
  2. **Structured input → branded PDF** via the **typst templates**
     (`typst_letter`, `typst_onepager`, `typst_presentation`), which additionally
     emit an **editable** `.pptx` (presentation) or `.docx` (letter/one-pager).
- **Converting uploads:** `convert_document` (LibreOffice) turns an uploaded
  Office/PDF file into `pdf` / `docx` / `txt` / `html` / per-page `images`.
- **The main gap:** there is **no faithful, structure-preserving converter from
  an uploaded `.pptx`/`.docx` into our branded typst templates**. That path is
  the LLM *re-authoring* the content into a template — interpretive, and **not
  guaranteed accurate**. See [Gaps](#gaps--limitations).

## The tools

| Tool | Direction | Engine | What it does |
|---|---|---|---|
| `fetch_attachment` | read upload → model | in-proc + pdfium | Text-ish files → UTF-8; images → viewable; **PDF** → text layer, or rasterized pages for a vision model. **Does not parse Office formats.** |
| `convert_document` | upload → file | LibreOffice (`soffice`) | Uploaded Office/PDF → `pdf`/`docx`/`txt`/`html`/`images` (one PNG per page). This is the way to get an uploaded `.docx`/`.pptx` content *into* the model (as `txt` or `images`). |
| `generate_document` | Markdown → file | pandoc (+weasyprint for PDF) | Markdown → `pdf`/`docx`/`pptx`. Generic, quick, **unbranded**. |
| `typst_letter` / `typst_onepager` / `typst_presentation` | structured input → file | typst | Renders a **branded** PDF + PNG preview from operator-defined templates. Also emits an editable export (below). |
| — editable **PPTX** (presentation) | typst → pptx | **typ2pptx** | Direct typst→PowerPoint: real editable text/shapes/gradients. Post-processed with a font fixup + shrink-to-fit + **embedded fonts** (see limitations). |
| — editable **DOCX** (letter/one-pager) | typst → PDF → docx | **pdf2docx** + LibreOffice | The rendered PDF is reconstructed into an editable Word doc. Layout-preserving but *flowing* (see limitations). |
| `create_document` / `edit_document` / `export_document` | canvas → file | in-proc + pandoc | Build a Markdown "canvas" doc across turns; export to pdf/docx/pptx. |
| `run_in_sandbox` | anything | Python/bash in sandbox | Escape hatch: pandas, PyMuPDF, python-docx/pptx, LibreOffice, etc. for bespoke conversions. |

All the file-producing paths run **inside the tool** — the model calls one tool
and gets the finished file(s) attached; there is no separate "now convert it"
step.

## Conversion paths

Each path below: **how it's done**, and the **expectation / limitation**.

### Reading what a user uploaded

| Upload | How | Expectation / limitation |
|---|---|---|
| `.md`, `.txt`, `.csv`, `.json`, code | `fetch_attachment` → UTF-8 text | Full fidelity; the model sees the raw text. |
| `.pdf` | `fetch_attachment` (text tier; image tier for scans) | Text-layer PDFs read cleanly; scanned PDFs fall back to a vision model per page. Layout/table structure is approximate. |
| images | `fetch_attachment` → inline image for a vision model | Model can *see* it; no OCR-to-text unless it reads the image. |
| `.docx`, `.pptx`, `.xlsx`, `.odt`, … | **`convert_document`** → `txt` (content) or `images` (look) | `fetch_attachment` returns only metadata for these ("re-upload" note). Route through `convert_document`: `txt` gives editable text (loses layout), `images` gives a faithful per-page render (loses editability). |

### Producing a document

| Goal | How | Expectation / limitation |
|---|---|---|
| Markdown → **PDF** | `generate_document(format=pdf)` | Clean, generic PDF (weasyprint). No croit branding. |
| Markdown → **DOCX/PPTX** | `generate_document(format=docx\|pptx)` | Editable, but **basic** — pandoc's default styling, not our brand. pptx from markdown is one-text-block-per-slide. |
| Branded **letter** → PDF | `typst_letter` | Pixel-perfect croit letterhead (logo, footer, register/VAT). Model supplies recipient + body only. |
| Branded **one-pager** → PDF | `typst_onepager` | Pixel-perfect; supports headings, bullet lists, tables. |
| Branded **deck** → PDF | `typst_presentation` | Pixel-perfect croit slides from a `deck.json` structure. |
| Deck → **editable PPTX** | `typst_presentation` (auto, via typ2pptx) | Editable text/shapes. **Fonts are embedded** so it renders correctly without Urbanist installed; text bodies use **shrink-to-fit** so a renderer's metric differences can't overflow. Brand rules must be gradient **fills**, not gradient line **strokes** (typ2pptx drops the latter). |
| Letter / one-pager → **editable DOCX** | `typst_*` (auto, via pdf2docx) | Editable Word text + logo + brand bars. It's a **flowing** reconstruction of a fixed layout, so expect minor drift (see limitations). |
| Uploaded Office/PDF → **PDF** | `convert_document(target=pdf)` | Faithful to the *original* file's look (LibreOffice). Keeps the source styling, **not** ours. |

## Typical office use cases

| User asks | Path | Status |
|---|---|---|
| "Turn my Markdown into a PDF." | `fetch_attachment` (or inline) → `generate_document(pdf)` | ✅ Works. |
| "Convert this `.docx` to PDF." | `convert_document(pdf)` | ✅ Works (LibreOffice; keeps source styling). |
| "Make a branded croit **letter** from this." | read content (`fetch_attachment` for md/txt; `convert_document(txt)` for docx) → model maps recipient/body → `typst_letter` | ◐ Works, but **LLM-mediated**: the model decides what maps to recipient/subject/body. Not a structural conversion. |
| "Make a branded **one-pager** from this text." | read content → `typst_onepager` | ◐ Same — model re-authors the content into fields. |
| "I need an editable Word/PowerPoint of this branded doc." | `typst_*` auto-emits `.docx`/`.pptx` | ✅ Works (with the fidelity caveats below). |
| "Convert my `.pptx` into **our** presentation style, without losing content." | `convert_document(txt\|images)` → model re-authors `deck.json` → `typst_presentation` | ✗ **Not accurate.** No structural pptx→template converter exists; the model rebuilds the deck from extracted text/images and *will* drop or reinterpret content. See gap #1. |
| "Extract the text/tables from this PDF/docx." | `fetch_attachment` (PDF) / `convert_document(txt)` (docx) | ✅ Text yes; complex tables approximate. |
| "Summarize/critique this uploaded deck." | `convert_document(images)` → vision model reads slides | ✅ Works for *understanding*; not for editing. |

## Gaps & limitations

Ordered by how much they bite.

1. **No faithful upload→template conversion (esp. `.pptx` → our presentation).**
   The user's "convert my pptx into our styling without losing content" is the
   biggest gap. `convert_document` can turn an uploaded pptx into `txt` (loses
   layout/structure) or `images` (loses editability), and the model can then
   *re-author* a `deck.json` — but that is interpretation, not conversion, and
   **cannot be relied on for accuracy**. A faithful path would need a real
   `pptx → deck.json` mapper (parse shapes/text/order via python-pptx, map to our
   slide layouts) — which does not exist. Same, less acutely, for `.docx` → letter.

2. **`fetch_attachment` can't read Office files.** A `.docx`/`.pptx`/`.xlsx`
   upload returns only metadata; the model must know to route it through
   `convert_document`. A user (or the model) expecting "read my docx" gets a
   "re-upload" note instead. Candidate fix: have `fetch_attachment` auto-route
   Office formats through the same LibreOffice `txt`/`images` conversion.

3. **Editable DOCX (letter/one-pager) is a flowing reconstruction.** pdf2docx
   rebuilds the PDF as Word content, so: the pinned footer can reflow to a 2nd
   page, the recipient block can wrap in a narrow box, and **fonts are not
   embedded** in the `.docx` (it degrades gracefully to a substitute, since it's
   flowing text). Good for editing; not byte-identical to the PDF.

4. **Editable PPTX is renderer-sensitive by nature.** typ2pptx positions text
   using typst's own shaping; another renderer's metrics differ. Mitigated by
   embedding fonts + shrink-to-fit autofit, but text-heavy *flowing* content
   (e.g. a letter) is mangled — typ2pptx is for slides only. Brand rules must be
   gradient **fills** (a thin rect), never gradient line **strokes**, or they
   vanish on export.

5. **Markdown→DOCX/PPTX via pandoc is unbranded and basic.** Fine for a quick
   throwaway; not a substitute for the typst templates when brand fidelity
   matters.

6. **Spreadsheets are read-only-ish.** `.xlsx` → `txt`/`pdf` via
   `convert_document`, or parsed with pandas in `run_in_sandbox`; there is no
   branded spreadsheet output path.

## Rules of thumb

- **Brand fidelity matters → typst template.** Editable Office needed → the
  template's auto `.pptx`/`.docx` export.
- **Just a PDF of an existing file → `convert_document`.** Just a PDF from
  Markdown → `generate_document`.
- **Need the model to *understand* an Office upload → `convert_document(images)`
  (to see) or `(txt)` (to read).**
- **"Accurate, no content loss" from an arbitrary uploaded deck into our
  template is not currently guaranteed** — set that expectation, or build the
  structural mapper in gap #1.
