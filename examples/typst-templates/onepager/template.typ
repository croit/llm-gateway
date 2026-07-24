// Example branded one-pager — neutral sample branding.
// This is a generic, data-free SAMPLE template shipped with the gateway to
// demonstrate a non-letter branded document. Replace the colours and logo
// below with your own, or point `[typst].templates_dir` at your own templates
// directory (see docs / gateway.example.toml).
//
// A NON-letter branded document: title + optional subtitle + body content, with
// a logo header and a generic page footer. The model supplies content + one
// optional switch:
//   language = "en" (default) | "de"   (typography only)
// Values arrive as strings on `sys.inputs`.

#let inputs = sys.inputs

// ---- Brand tokens (sample palette — swap for your own) ---------------------
#let c-primary = rgb("#2563EB")
#let c-accent = rgb("#0EA5E9")
#let ink = rgb("#0F172A")
#let muted = rgb("#64748B")
#let brand = gradient.linear(c-primary, c-accent)

// ---- Switches --------------------------------------------------------------
// Document language, for correct hyphenation/justification only ("en" default,
// "de" for a German one-pager). The footer is generic, so no entity switch.
#let lang = inputs.at("language", default: "en")

// ---- Optional content image ------------------------------------------------
// The model may place ONE image in the one-pager (a photo, chart, diagram,
// screenshot, logo, …) at a chosen size, flowing in the page — never
// overlapping the text. `image` is an `att:<turn>/<file>` attachment ref (or a
// template-relative asset path); `image_size` picks the width; `image_position`
// puts it above or below the body.
#let doc-image = inputs.at("image", default: "")
#let img-width = (
  small: 40%, medium: 65%, large: 85%, full: 100%,
).at(inputs.at("image_size", default: "large"), default: 85%)
#let img-caption = inputs.at("image_caption", default: "")
#let img-pos = inputs.at("image_position", default: "bottom")
#let render-doc-image() = {
  if doc-image != "" {
    v(6mm)
    align(center, image(doc-image, width: img-width))
    if img-caption != "" {
      v(2mm)
      align(center, text(size: 9pt, fill: muted, style: "italic")[#img-caption])
    }
  }
}

// ---- Page (generic brand footer with page numbers) -------------------------
#set page(
  paper: "a4",
  margin: (left: 22mm, right: 22mm, top: 18mm, bottom: 26mm),
  footer: [
    #image("assets/brand-bar.png", width: 100%, height: 1pt)
    #v(3pt)
    #set text(size: 8pt, fill: muted)
    #set par(justify: false, leading: 0.5em)
    #grid(
      columns: (1fr, auto),
      align: (left + horizon, right + horizon),
      text(weight: "semibold", fill: c-primary)[© Example Corp],
      // `context` so `final()` can see the last page — "Page 1 of 3".
      context [Page #counter(page).display() of #counter(page).final().first()],
    )
  ],
  footer-descent: 4mm,
)
#set text(font: "Urbanist", size: 11pt, fill: ink, lang: lang)
#set par(justify: true, leading: 0.72em, spacing: 1.15em)

// ---- Brand styling for the body's native Typst elements --------------------
// The body is rendered with eval(mode: "markup") below; these rules make
// Typst's own headings / links / quotes / lists / tables come out in the sample
// brand automatically, so the model just writes plain Typst markup and never
// sets colours itself. Swap the colours for your own.
#let heading-sizes = (15pt, 13pt, 11.5pt)
#show heading: it => {
  set text(fill: c-primary, weight: "bold", size: heading-sizes.at(calc.min(it.level, 3) - 1))
  block(above: if it.level == 1 { 4mm } else { 3mm }, below: 2mm, it.body)
}
#show link: it => underline(text(fill: c-primary, it))
#show quote.where(block: true): it => block(
  width: 100%, above: 8pt, below: 8pt,
  inset: (left: 12pt, top: 4pt, bottom: 4pt),
  stroke: (left: 3pt + c-primary),
  it.body,
)
#set enum(numbering: n => text(fill: c-primary, weight: 600)[#n.])
#set list(marker: (text(fill: c-primary)[•], text(fill: c-primary)[‣], text(fill: c-primary)[·]))
// Tables the model writes with #table(...) get the sample look (brand header
// row, light zebra body, hairline rules). Works with or without table.header.
#set table(
  inset: (x: 8pt, y: 5pt), align: left + horizon,
  stroke: (y: 0.5pt + rgb("#DCE4F7")),
  fill: (_, row) => if row == 0 { c-primary } else if calc.odd(row) { rgb("#EFF4FE") } else { white },
)
#show table.cell: it => if it.y == 0 {
  text(fill: white, weight: "semibold", it)
} else { it }

// ---- Header: logo + brand gradient rule ------------------------------------
#image("assets/logo.svg", height: 10mm)
#v(6pt)
#image("assets/brand-bar.png", width: 100%, height: 2pt)

#v(9mm)

// ---- Title + optional subtitle ---------------------------------------------
#text(weight: "bold", size: 22pt, fill: ink)[#inputs.title]
#let subtitle = inputs.at("subtitle", default: "")
#if subtitle != "" {
  v(2mm)
  text(weight: "medium", size: 13pt, fill: c-primary)[#subtitle]
}

#v(7mm)

// Content image above the body when requested.
#if img-pos == "top" { render-doc-image(); v(7mm) }

// ---- Body: native Typst markup ---------------------------------------------
// Rendered with eval(mode: "markup"): *bold*, _italic_, `= headings`, `- ` / `+ `
// lists, #link(...), #table(...) all style themselves via the show rules above.
// Paragraphs stay blank-line separated; a lone newline is a space.
#eval(inputs.body, mode: "markup")

// Content image below the body (the default position).
#if img-pos != "top" { render-doc-image() }
