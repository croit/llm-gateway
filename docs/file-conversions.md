# File conversions

How the gateway reads uploaded files and produces documents in other formats,
which tool/engine does each job, and — importantly — what each path can and
**cannot** do. Written for operators and for reasoning about feature gaps.

## TL;DR

- **Reading uploads:** text-ish files and PDFs are read natively; Office files
  (`.docx`/`.pptx`/`.xlsx`) are read by `fetch_attachment` as **verbatim
  structured content** (a sandbox python extractor: titles, text, bullets,
  tables, notes) and any embedded images are re-attached as `att:` refs the
  model can carry into a render.
- **Producing documents:** two families —
  1. **Markdown → PDF/DOCX/PPTX** via `generate_document` (pandoc). Fast, generic,
     unbranded.
  2. **Structured input → branded PDF** via the **typst templates**
     (`typst_letter`, `typst_onepager`, `typst_presentation`), which additionally
     emit an **editable** `.pptx` (presentation) or `.docx` (letter/one-pager).
- **Converting uploads:** `convert_document` (LibreOffice) turns an uploaded
  Office/PDF file into `pdf` / `docx` / `txt` / `html` / per-page `images`.
- **Upload → our template:** `fetch_attachment` extracts the content verbatim and
  hands back image refs; the **model maps** that content onto our slide layouts /
  letter fields (a deliberate design choice — layout mapping is judgment a tool
  can't do well). Content (text + images) is preserved; the *layout* is
  re-expressed in our style, which is the point. See [Gaps](#gaps--limitations).

## The tools

| Tool | Direction | Engine | What it does |
|---|---|---|---|
| `fetch_attachment` | read upload → model | in-proc + pdfium + sandbox | Text-ish files → UTF-8; images → viewable; **PDF** → text layer, or rasterized pages for a vision model; **Office** (`.docx`/`.pptx`/`.xlsx`) → **verbatim structured JSON** (python-pptx/docx/openpyxl in the sandbox) with embedded images re-attached as `att:` refs. |
| `convert_document` | upload → file | LibreOffice (`soffice`) | Uploaded Office/PDF → `pdf`/`docx`/`txt`/`html`/`images` (one PNG per page). Use for the *look* (`images`) or a plain-PDF of the original; `fetch_attachment` is now the better route for the *content*. |
| `generate_document` | Markdown → file | pandoc (+weasyprint for PDF) | Markdown → `pdf`/`docx`/`pptx`. Generic, quick, **unbranded**. |
| `typst_letter` / `typst_onepager` / `typst_presentation` | structured input → file | typst | Renders a **branded** PDF + PNG preview from operator-defined templates. Also emits an editable export (below). |
| — editable **PPTX** (presentation) | typst → pptx | **typ2pptx** | Direct typst→PowerPoint: real editable text/shapes/gradients. Post-processed with a font fixup + shrink-to-fit + **embedded fonts** (see limitations). |
| — editable **DOCX** (letter/one-pager) | typst → HTML → docx | **pandoc** (+ python-docx) | The template is compiled to HTML and converted to editable Word by pandoc; the `[docx] font` is set as the document default and embedded as `.odttf` (from the template's own `fonts/`). Genuinely editable, on-brand text; fixed-layout chrome (a `place()`d footer) is dropped — see limitations. |
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
| `.docx`, `.pptx`, `.xlsx` | **`fetch_attachment`** → verbatim structured JSON (+ `att:` image refs) | The primary path: a sandbox extractor returns titles/text/bullets/tables/notes with **no rewording**, and embedded images come back as `att:` refs to drop into a render. Use `convert_document(images)` on top only when the model needs to *see* the original layout. `.odt` and other Office variants still go via `convert_document`. |

### Producing a document

| Goal | How | Expectation / limitation |
|---|---|---|
| Markdown → **PDF** | `generate_document(format=pdf)` | Clean, generic PDF (weasyprint). No croit branding. |
| Markdown → **DOCX/PPTX** | `generate_document(format=docx\|pptx)` | Editable, but **basic** — pandoc's default styling, not our brand. pptx from markdown is one-text-block-per-slide. |
| Branded **letter** → PDF | `typst_letter` | Pixel-perfect croit letterhead (logo, footer, register/VAT). Model supplies recipient + body only. |
| Branded **one-pager** → PDF | `typst_onepager` | Pixel-perfect; supports headings, bullet lists, tables. |
| Branded **deck** → PDF | `typst_presentation` | Pixel-perfect croit slides from a `deck.json` structure. |
| Deck → **editable PPTX** | `typst_presentation` (auto, via typ2pptx) | Editable text/shapes. **Fonts are embedded** so it renders correctly without Urbanist installed; text bodies use **shrink-to-fit** so a renderer's metric differences can't overflow. Brand rules must be gradient **fills**, not gradient line **strokes** (typ2pptx drops the latter). |
| Uploaded images → **into a render** | `att:` ref in any image field | Images that `fetch_attachment` pulled out of an upload (its `image_refs`) are carried into a render by pasting the `att:<turn>/<file>` ref into an image/bg_image/avatar field. The renderer fetches the bytes and **stages** them under `uploads/` in the compile root (and the pptx bundle), so `image("uploads/…")` resolves. Refs are re-staged on every render/edit — never persisted as paths. |
| Letter / one-pager → **editable DOCX** | `typst_*` (auto, via typst→HTML→pandoc) | Editable Word text + logo + brand bar + headings/lists/tables, in the brand font (embedded). Content, not fixed layout — a `place()`d footer is dropped (see limitations). |
| Uploaded Office/PDF → **PDF** | `convert_document(target=pdf)` | Faithful to the *original* file's look (LibreOffice). Keeps the source styling, **not** ours. |

## Typical office use cases

| User asks | Path | Status |
|---|---|---|
| "Turn my Markdown into a PDF." | `fetch_attachment` (or inline) → `generate_document(pdf)` | ✅ Works. |
| "Convert this `.docx` to PDF." | `convert_document(pdf)` | ✅ Works (LibreOffice; keeps source styling). |
| "Make a branded croit **letter** from this." | `fetch_attachment` (verbatim content) → model maps recipient/body → `typst_letter` | ◐ Works, but **LLM-mediated**: the model decides what maps to recipient/subject/body. Content is verbatim; the mapping is judgment. |
| "Make a branded **one-pager** from this text." | `fetch_attachment` → `typst_onepager` | ◐ Same — model re-authors the content into fields. |
| "I need an editable Word/PowerPoint of this branded doc." | `typst_*` auto-emits `.docx`/`.pptx` | ✅ Works (with the fidelity caveats below). |
| "Convert my `.pptx` into **our** presentation style, without losing content." | `fetch_attachment` (verbatim slides + `att:` image refs) → model maps each slide onto a croit layout, carrying images via their refs → `typst_presentation` | ◐ **Content preserved, layout re-styled.** Text/tables/notes come back verbatim and images are carried through — nothing is dropped. What is *not* 1:1 is the layout: the model chooses the closest croit layout per slide (by design — that's the migration). Not a pixel-copy of the source. |
| "Extract the text/tables from this PDF/docx." | `fetch_attachment` (PDF text tier; Office structured JSON) | ✅ Verbatim text/tables; PDF table structure approximate. |
| "Summarize/critique this uploaded deck." | `convert_document(images)` → vision model reads slides | ✅ Works for *understanding*; not for editing. |

## Gaps & limitations

Ordered by how much they bite.

1. **Upload→template is content-faithful but layout is the LLM's call.**
   `fetch_attachment` now returns an uploaded deck/doc as **verbatim structured
   content** (python-pptx/docx/openpyxl, no rewording) plus `att:` refs for every
   embedded image, and the presentation renderer stages those images. So the
   *content* (text, tables, notes, images) survives a `.pptx → our presentation`
   migration. What is deliberately **not** preserved is the source *layout*: the
   model picks the closest croit slide layout per slide. This is by design — a
   tool can't make good layout-mapping decisions, and re-styling into our system
   is the whole point. Residual risk is the usual LLM one (a mis-mapped layout,
   an image dropped from a field), not a lossy extraction step. Same shape for
   `.docx` → letter/one-pager.

2. **Editable DOCX (letter/one-pager) carries content, not fixed layout.**
   typst can't emit `.docx`, so the export compiles the template to HTML and
   converts *that* with pandoc. Body text, headings, lists, tables, and the
   logo/brand-bar images come through as editable Word content, in the brand
   font (`[docx] font`, set as the default and embedded as `.odttf` from the
   template's `fonts/`, so it renders on-brand even without the font installed).
   The legal footer (register/VAT/bank) is a `place()`d element that HTML export
   drops, so the letter template re-emits it as a flowing block for the HTML
   target (`#context if target() == "html"`) — keeping it in the Word/ODT
   output. What's still **lost**: other fixed-layout chrome, and per-element
   font variation collapses to the one `[docx] font`. It's an on-brand editable
   draft, not a byte-match of the PDF. A `.odt` is emitted alongside the `.docx`
   (same pandoc HTML source) for LibreOffice/OpenOffice users. Only text-centric
   templates should opt into `[docx]`; layout/graphics-heavy ones (decks) use
   `[pptx]` instead.

3. **Editable PPTX is renderer-sensitive by nature.** typ2pptx positions text
   using typst's own shaping; another renderer's metrics differ. Mitigated by
   embedding fonts + shrink-to-fit autofit, but text-heavy *flowing* content
   (e.g. a letter) is mangled — typ2pptx is for slides only. Brand rules must be
   gradient **fills** (a thin rect), never gradient line **strokes**, or they
   vanish on export.

4. **Markdown→DOCX/PPTX via pandoc is unbranded and basic.** Fine for a quick
   throwaway; not a substitute for the typst templates when brand fidelity
   matters.

5. **Spreadsheets are read-only-ish.** `.xlsx` → `txt`/`pdf` via
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
