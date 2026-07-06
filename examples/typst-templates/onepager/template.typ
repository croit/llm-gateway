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

// ---- Page (generic brand footer with page numbers) -------------------------
#set page(
  paper: "a4",
  margin: (left: 22mm, right: 22mm, top: 18mm, bottom: 26mm),
  footer: [
    #rect(width: 100%, height: 1pt, fill: brand, stroke: none)
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

// ---- Header: logo + brand gradient rule ------------------------------------
#image("assets/logo.svg", height: 10mm)
#v(6pt)
#rect(width: 100%, height: 2pt, fill: brand, stroke: none)

#v(9mm)

// ---- Title + optional subtitle ---------------------------------------------
#text(weight: "bold", size: 22pt, fill: ink)[#inputs.title]
#let subtitle = inputs.at("subtitle", default: "")
#if subtitle != "" {
  v(2mm)
  text(weight: "medium", size: 13pt, fill: c-primary)[#subtitle]
}

#v(7mm)

// ---- Inline formatting: **bold** and *italic* ------------------------------
// Split on the delimiter and emphasise the ODD segments (text between a matched
// pair). Unbalanced markers degrade gracefully rather than printing literally.
#let format-italic(s) = s.split("*").enumerate().map(((i, part)) => {
  if calc.odd(i) { emph(part) } else { part }
}).join()
#let format-inline(s) = s.split("**").enumerate().map(((i, part)) => {
  if calc.odd(i) { strong(format-italic(part)) } else { format-italic(part) }
}).join()

// ---- Tables inside the body ------------------------------------------------
// A block is a table when it has ≥2 non-empty lines and EVERY non-empty line
// contains a "|" — prose practically never does.
#let table-lines(block) = block.split("\n").map(l => l.trim()).filter(l => l != "")
#let is-table(block) = {
  let lines = table-lines(block)
  lines.len() >= 2 and lines.all(l => l.contains("|"))
}
#let is-sep(line) = {
  let cells = line.trim("|").split("|").map(c => c.trim()).filter(c => c != "")
  cells.len() > 0 and cells.all(c => c.match(regex("^:?-{2,}:?$")) != none)
}
#let parse-row(line) = line.trim("|").split("|").map(c => c.trim())
#let render-table(block) = {
  let rows = table-lines(block).filter(l => not is-sep(l)).map(parse-row)
  if rows.len() == 0 { return }
  let ncols = rows.first().len()
  let fix(row) = if row.len() < ncols {
    row + (("",) * (ncols - row.len()))
  } else {
    row.slice(0, ncols)
  }
  let header = fix(rows.first())
  let body-rows = rows.slice(1).map(fix)
  set text(size: 10pt)
  set par(justify: false)
  table(
    columns: ncols,
    inset: (x: 8pt, y: 5pt),
    align: left + horizon,
    stroke: (y: 0.5pt + rgb("#DCE4F7")),
    fill: (_, row) => if row == 0 { c-primary } else if calc.odd(row) { rgb("#EFF4FE") } else { white },
    table.header(..header.map(c => text(fill: white, weight: "semibold")[#format-inline(c)])),
    ..body-rows.map(r => r.map(c => [#format-inline(c)])).flatten(),
  )
}

// ---- Prose / headings / bullet lists ---------------------------------------
// Unlike the letter, a one-pager supports bullet lists: a run of lines starting
// "- " or "* " becomes a list. Heading lines ("#".."###") print in the brand
// colour; runs of plain lines join into one justified paragraph.
#let heading-re = regex("^(#{1,3})\\s+(.+)$")
#let bullet-re = regex("^[-*]\\s+(.+)$")
#let heading-sizes = (15pt, 13pt, 11.5pt)
#let render-prose(blk) = {
  let buf = ()          // pending plain-prose lines
  let items = ()        // pending bullet-list items
  for line in blk.split("\n") {
    let t = line.trim()
    let hm = t.match(heading-re)
    let bm = t.match(bullet-re)
    if hm != none {
      if buf.len() > 0 { format-inline(buf.join(" ")); parbreak(); buf = () }
      if items.len() > 0 { list(..items.map(it => format-inline(it))); items = () }
      let level = hm.captures.at(0).len()
      v(if level == 1 { 3mm } else { 2mm })
      text(weight: "bold", size: heading-sizes.at(level - 1), fill: c-primary,
        format-inline(hm.captures.at(1)))
      parbreak()
    } else if bm != none {
      if buf.len() > 0 { format-inline(buf.join(" ")); parbreak(); buf = () }
      items.push(bm.captures.at(0))
    } else if t == "" {
      // blank line within a block: separate prose runs / end a list
      if buf.len() > 0 { format-inline(buf.join(" ")); parbreak(); buf = () }
      if items.len() > 0 { list(..items.map(it => format-inline(it))); items = () }
    } else {
      if items.len() > 0 { list(..items.map(it => format-inline(it))); items = () }
      buf.push(line)
    }
  }
  if buf.len() > 0 { format-inline(buf.join(" ")) }
  if items.len() > 0 { list(..items.map(it => format-inline(it))) }
}

#let paras = inputs.body.split("\n\n")
#for block in paras {
  if is-table(block) {
    render-table(block)
  } else {
    render-prose(block)
  }
  parbreak()
}
