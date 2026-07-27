// Example corporate presentation — 16:9 slide deck, neutral sample branding.
// This is a generic, data-free SAMPLE template shipped with the gateway to
// demonstrate a slide-deck document. Replace the palette, logo and fonts below
// with your own, or point `[typst].templates_dir` at your own templates
// directory (see docs / gateway.example.toml).
//
// CONTENT IS FULLY SEPARATE: this template is never edited. The model only
// writes a JSON deck (default `deck.json`) describing the slides; this renderer
// turns it into branded slides. Compile with:
//   typst compile template.typ deck.pdf --font-path fonts --input data=deck.json
//
// Each slide object has a `layout` (cover|agenda|section|content|split|cards|
// stats|comparison|process|quote|media|team|table|diagram|closing, …) plus the
// fields that layout needs (the full catalogue lives in template.toml's
// description). Brand (Urbanist, dark/white surfaces, the sample gradient, glass
// cards, 18pt radius, logo SVG) is applied automatically here. This sample ships
// no background art: cover/section/statement render on the plain themed
// background unless the deck sets a `bg_image`.

// Content arrives one of two ways, both fully separate from this template:
//   • Gateway / tool use: `--input deck=<JSON string>` (the whole deck inline)
//   • Local dev:          `--input data=<path.json>` (defaults to deck.json)
#let _inline = sys.inputs.at("deck", default: none)
#let deck = if _inline != none { json(bytes(_inline)) } else { json(sys.inputs.at("data", default: "deck.json")) }
#let slides = deck.at("slides", default: ())
#let deck-title = deck.at("deck_title", default: "")
#let footer-left = deck.at("footer_left", default: "example.com")

// ---- Brand tokens (Example Visual Identity V.02) -----------------------------
#let anthracite = rgb("#1D1D1B")
#let purple = rgb("#2563EB")
#let peach = rgb("#0EA5E9")
#let dark-purple = rgb("#1E40AF")
#let teal = rgb("#0A889A")
#let lightgray = rgb("#F6F6F6")
#let mgray = rgb("#888888")
// Direction matches example.com: Purple → Peach, left to right (the CI gradient
// swatches list Purple first; the live site renders `to right, #2563EB → #0EA5E9`).
#let grad = gradient.linear(purple, peach, angle: 0deg)
#let grad-soft = gradient.linear(purple.transparentize(70%), peach.transparentize(70%), angle: 0deg)

// ---- Geometry (standard 16:9, 13.333in x 7.5in) ----------------------------
#let SW = 33.867cm
#let SH = 19.05cm
#let PADX = 1.7cm // body left/right padding
#let PADT = 2.9cm // body top (clears logo)
#let PADB = 1.7cm // body bottom (clears footer)
#let CW = SW - 2 * PADX // usable content width
// Usable content height. Needed explicitly because `height: 100%` inside `pad`
// resolves against the region BEFORE the bottom padding is taken off, so a
// full-height box would reach down through the footer. The extra clearance is
// because PADB alone does NOT clear the footer: the rule sits a hair *inside*
// the nominal content box, which only shows up when something is pinned to the
// very bottom of it (a caption under a full-height image was landing on the rule).
#let CH = SH - PADT - PADB - 0.55cm

// ---- Per-theme palette -----------------------------------------------------
#let palette(theme) = if theme == "light" {
  (
    bg: white, fg: anthracite, fg2: rgb("#4A4A4A"), muted: mgray,
    logo: "assets/logo-dark.svg",
    card-fill: white, card-line: rgb(0, 0, 0, 18),
    card-inner: white, zebra: lightgray, dark: false,
    icon-hex: "#1D1D1B",
  )
} else {
  (
    bg: anthracite, fg: white, fg2: rgb(255, 255, 255, 200), muted: mgray,
    logo: "assets/logo-light.svg",
    card-fill: rgb(255, 255, 255, 16), card-line: rgb(255, 255, 255, 46),
    card-inner: anthracite, zebra: rgb(255, 255, 255, 10), dark: true,
    icon-hex: "#FFFFFF",
  )
}

// Outline icons (Lucide-style). The `icon` field on a card names one of these;
// the SVG ships with stroke="currentColor", recoloured per theme here. Icons are
// white on dark / anthracite on light, outline-only — per the Example CI.
#let icon-names = (
  "server", "activity", "trending-up", "shield", "database", "bell", "users",
  "zap", "cloud", "lock", "layers", "clock", "gauge", "globe", "rocket", "check-circle",
  "arrow-up-right",
)
// ---- Button system (mirrors example.com) --------------------------------------
// Exact site button gradient (#1E40AF → #0EA5E9, warmer than the CI accent).
#let cta-grad = gradient.linear(rgb("#1E40AF"), rgb("#0EA5E9"), angle: 0deg)
#let btn-arrow(hex, size: 14pt) = box(baseline: 18%, image(
  bytes(read("assets/icons/arrow-up-right.svg").replace("currentColor", hex)), format: "svg", height: size))
// Three styles, all 12pt radius + arrow, per the site:
//   primary  = gradient pill, white label        (use on dark backgrounds)
//   light    = white pill, purple label           (use on gradient panels)
//   outline  = transparent pill, white border/label (secondary, on gradient/dark)
#let btn(label, style: "primary") = {
  let inset = (x: 20pt, y: 10pt)
  let row(txtcol, arrowhex) = grid(columns: (auto, auto), column-gutter: 9pt, align: horizon,
    text(size: 14pt, weight: 500, fill: txtcol)[#label], btn-arrow(arrowhex))
  if style == "light" {
    box(fill: white, radius: 12pt, inset: inset, row(purple, "#2563EB"))
  } else if style == "outline" {
    box(stroke: 1pt + rgb(255, 255, 255, 150), radius: 12pt, inset: inset, row(white, "#FFFFFF"))
  } else {
    box(fill: cta-grad, radius: 12pt, inset: inset, row(white, "#FFFFFF"))
  }
}
// normalise `buttons` entries: a string → {label}, else the object as-is
#let norm-btns(arr) = arr.map(b => if type(b) == str { (label: b) } else { b })
#let button-row(arr, default-style: "primary") = {
  let bs = norm-btns(arr)
  grid(columns: bs.map(_ => auto), column-gutter: 12pt, align: horizon,
    ..bs.map(b => btn(b.at("label", default: ""), style: b.at("style", default: default-style))))
}
// backward-compatible single CTA (gradient pill)
#let cta-button(label) = btn(label, style: "primary")
#let lucide(name, hex, size) = box(height: size, baseline: 25%, image(
  bytes(read("assets/icons/" + name + ".svg").replace("currentColor", hex)),
  format: "svg", height: size,
))

// ---- Shared atoms ----------------------------------------------------------
#let logo-img(p, h: 0.62cm) = image(p.logo, height: h)
#let kicker(p, s) = {
  let k = s.at("kicker", default: "")
  if k != "" {
    align(right, text(size: 11pt, weight: 600, tracking: 1.5pt, fill: purple)[#upper(k)])
  }
}
#let title-block(p, s) = {
  set par(leading: 0.35em)
  text(size: 30pt, weight: 600, fill: p.fg)[#s.at("title", default: "")]
  let sub = s.at("subtitle", default: "")
  if sub != "" {
    v(2pt)
    text(size: 15pt, fill: p.fg2)[#sub]
  }
}
#let grad-text(body, size: 30pt, weight: 600) = text(
  size: size, weight: weight, fill: grad,
)[#body]

// glass card
#let glass(p, body, inset: 18pt) = block(
  fill: p.card-fill, stroke: 0.7pt + p.card-line, radius: 18pt,
  inset: inset, width: 100%, height: 100%,
)[#body]
// gradient-border card (the "highlight")
#let grad-card(p, body, inset: 18pt) = block(
  fill: grad, radius: 18pt, inset: 1.2pt, width: 100%, height: 100%,
)[#block(fill: p.card-inner, radius: 17pt, inset: inset, width: 100%, height: 100%)[#body]]

// small pill badge / tag (glass chip) — restrained, corporate
#let badge(p, label) = box(fill: p.card-fill, stroke: 0.6pt + p.card-line, radius: 100pt,
  inset: (x: 11pt, y: 5pt), text(size: 10.5pt, weight: 600, fill: p.fg2)[#label])
#let badge-row(p, tags) = grid(columns: tags.map(_ => auto), column-gutter: 8pt,
  ..tags.map(t => badge(p, t)))

// ---- Image placement -------------------------------------------------------
// Cropping a picture to fill its box ("cover") is free for a photo but
// destructive for a diagram, chart or screenshot: a 4:3 diagram in a 16:9 hole
// loses a quarter of its width, and what goes missing is usually a label. So we
// only crop when the picture is already about as wide as the box it has to fill,
// and letterbox it ("contain") otherwise — nothing is ever cut off silently.
#let AR-TOL = 0.20 // how far off the box's aspect a picture may be and still be cropped

// natural aspect ratio of an image file (0.0 if unknown); needs a `context`,
// because `measure` does.
#let img-ar(path) = {
  let nat = measure(image(path))
  if nat.height == 0pt { 0.0 } else { nat.width / nat.height }
}
// May `path` be cropped into a box of this aspect without losing anything that
// matters? A 16:9 photo in a 16:9 hole: yes. A 4:3 diagram: no.
#let croppable(path, box-ar) = {
  let ar = img-ar(path)
  ar == 0.0 or calc.abs(ar - box-ar) <= box-ar * AR-TOL
}
// Draw `path` into a full-width, `h`-tall box, cropping only when that costs
// (almost) nothing. `layout` supplies the box width the aspect test needs.
#let fitted-image(path, h) = layout(sz => context box(
  width: 100%, height: h, clip: true,
  image(path, width: 100%, height: 100%,
    fit: if croppable(path, sz.width / h) { "cover" } else { "contain" }),
))

// browser/device frame around a screenshot (neutral window chrome). `h` = media
// height; the frame adds a slim title bar with three muted dots above the image.
#let device-frame(img, h: 11cm) = block(radius: 12pt, clip: true, width: 100%,
  stroke: 0.8pt + rgb(255, 255, 255, 38), fill: rgb("#16171B"))[
  #block(width: 100%, fill: rgb("#202126"), inset: (x: 12pt, y: 8pt))[
    #grid(columns: (auto, auto, auto), column-gutter: 7pt,
      ..((rgb("#5A5A60"),) * 3).map(c => circle(radius: 3.5pt, fill: c, stroke: none)))
  ]
  #fitted-image(img, h)
]

// full-bleed background image + dark scrim (keeps text legible). Takes a
// resolved path: "" or "none" → nothing placed. This sample ships no background
// art, so cover/section/statement default to "" (plain themed background); a
// deck can set `bg_image` to its own image, or drop your brand backgrounds in
// and wire their paths as the fallbacks below.
#let resolve-bg(s, fallback) = {
  let bg = s.at("bg_image", default: fallback)
  if bg == "none" { "" } else { bg }
}
#let bg-scrim(bg, scrim: 40%) = {
  if bg != "" {
    place(top + left, image(bg, width: SW, height: SH, fit: "cover"))
    place(top + left, rect(width: SW, height: SH, fill: anthracite.transparentize(scrim)))
  }
}

// circular image avatar, or gradient initials when no avatar is given
#let avatar(p, person, d: 2.6cm) = {
  let av = person.at("avatar", default: "")
  if av != "" {
    box(width: d, height: d, block(radius: 50%, clip: true, width: 100%, height: 100%,
      image(av, width: 100%, height: 100%, fit: "cover")))
  } else {
    let initials = person.at("name", default: "?").split(" ").map(w => w.slice(0, 1)).join("")
    box(width: d, height: d, block(fill: grad, radius: 50%, inset: 0pt, width: 100%, height: 100%,
      align(center + horizon, text(size: 22pt, weight: 600, fill: white)[#initials])))
  }
}

// true circular number badge (gradient ring, inner fill, number)
#let num-badge(p, label, d: 1.3cm) = box(width: d, height: d, block(
  fill: grad, radius: 50%, inset: 1.4pt, width: 100%, height: 100%,
)[#block(fill: p.card-inner, radius: 50%, width: 100%, height: 100%, inset: 0pt,
    align(center + horizon, text(size: 15pt, weight: 600, fill: p.fg)[#label]))])

// the one canonical footer, shared by every slide for consistency
#let footer-row(p, n, on-grad: false) = {
  let txtcol = if on-grad { rgb(255, 255, 255, 220) } else { p.muted }
  // A gradient *fill* on a thin rect, not a gradient *stroke* on a line:
  // typ2pptx renders DrawingML gradient fills natively but drops gradient
  // line strokes, so the footer rule vanished on some exported slides.
  let rule-fill = if on-grad { rgb(255, 255, 255, 170) } else { grad }
  place(bottom + left, dx: PADX, dy: -0.85cm, box(width: CW)[
    #rect(width: 100%, height: 1pt, fill: rule-fill, stroke: none)
    #v(5pt)
    #grid(
      columns: (1fr, auto),
      text(size: 9pt, fill: txtcol)[#footer-left],
      text(size: 9pt, fill: txtcol)[#deck-title #h(6pt) · #h(6pt) #{ if n < 10 { "0" } }#str(n)],
    )
  ])
}

// chrome wrapper for standard content slides (logo + kicker + footer + body)
#let chrome(p, n, s, body) = {
  place(top + left, dx: PADX, dy: 1.15cm, logo-img(p))
  place(top + right, dx: -PADX, dy: 1.3cm, box(width: 14cm, kicker(p, s)))
  footer-row(p, n)
  pad(left: PADX, right: PADX, top: PADT, bottom: PADB, body)
}

// ---- Metric atoms (the `dashboard` layout) ---------------------------------
// A signed delta chip, coloured by direction: "+…" reads as up (green), "-…"
// (ASCII or the − minus sign) as down (peach), anything else muted. The sign
// lives in the model-supplied string, so no arrow glyph is needed (avoids
// missing-glyph tofu in Urbanist).
#let trend-chip(p, t) = {
  if t == "" { return [] }
  let up = t.starts-with("+")
  let down = t.starts-with("-") or t.starts-with("−")
  let col = if up { rgb("#38A76F") } else if down { peach } else { p.muted }
  text(size: 12.5pt, weight: 600, fill: col)[#t]
}
// A mini trend line for a metric card. Solid-colour stroke (NOT a gradient
// stroke — typ2pptx drops those, same reason the footer rule is a fill rect),
// so it survives the editable-pptx export. `data` is an array of numbers.
#let sparkline(data, h: 1.15cm, paint: purple) = {
  if type(data) != array or data.len() < 2 { return [] }
  let lo = calc.min(..data)
  let hi = calc.max(..data)
  let span = calc.max(hi - lo, 0.0001)
  layout(size => {
    let ww = size.width
    let n = data.len()
    let pts = data.enumerate().map(iv => (
      ww * (iv.at(0) / (n - 1)),
      h * (1 - (iv.at(1) - lo) / span),
    ))
    box(width: 100%, height: h, curve(
      stroke: 1.6pt + paint,
      curve.move(pts.first()),
      ..pts.slice(1).map(pt => curve.line(pt)),
    ))
  })
}

// ============================================================================
// LAYOUTS
// ============================================================================

#let l-cover(p, s) = {
  let bg = resolve-bg(s, "")
  let on-photo = bg != ""
  bg-scrim(bg, scrim: 32%)
  place(top + left, dx: PADX, dy: 1.3cm, logo-img(if on-photo { (logo: "assets/logo-light.svg") } else { p }, h: 0.7cm))
  // Over a grainient the title is solid WHITE (matches the CI section pages);
  // on a plain background it uses the brand gradient.
  let title-it(t, size: 48pt) = if on-photo { text(size: size, weight: 600, fill: white)[#t] } else { grad-text(t, size: size) }
  pad(left: PADX, right: PADX, top: 6.0cm, bottom: PADB, {
    let eb = s.at("eyebrow", default: "")
    if eb != "" {
      text(size: 12pt, weight: 600, tracking: 2pt, fill: if on-photo { rgb(255, 255, 255, 235) } else { purple })[#upper(eb)]
      v(10pt)
    }
    // explicit per-line blocks with a guaranteed gap — display type at this
    // size has deep descenders (g, comma) that overlap the next line's
    // ascenders if we lean on paragraph leading alone.
    set par(leading: 0.3em)
    block(below: 30pt, title-it(s.at("title", default: "")))
    let t2 = s.at("title_line_2", default: "")
    if t2 != "" { block(title-it(t2)) }
    let sub = s.at("subtitle", default: "")
    if sub != "" {
      v(14pt)
      text(size: 18pt, fill: if on-photo { rgb(255, 255, 255, 230) } else { p.fg2 })[#sub]
    }
  })
  place(bottom + left, dx: PADX, dy: -1.2cm, box(width: CW, grid(
    columns: (1fr, auto),
    text(size: 12pt, fill: p.fg)[
      #let nm = s.at("presenter", default: "")
      #let rl = s.at("role", default: "")
      #if nm != "" [#text(weight: 600)[#nm]#if rl != "" [ · #text(fill: p.muted)[#rl]]]
    ],
    text(size: 12pt, fill: p.muted)[#s.at("date", default: "")],
  )))
}

#let l-section(p, n, s) = {
  // plain themed background by default; set `bg_image` for your own art
  let bg = resolve-bg(s, "")
  let on-photo = bg != ""
  if on-photo { bg-scrim(bg, scrim: 42%) } else { place(top + left, rect(width: SW, height: SH, fill: grad-soft)) }
  place(top + left, dx: PADX, dy: 1.3cm, logo-img(if on-photo { (logo: "assets/logo-light.svg") } else { p }, h: 0.7cm))
  let tcol = if on-photo { white } else { p.fg }
  let scol = if on-photo { rgb(255, 255, 255, 230) } else { p.fg2 }
  pad(left: PADX, right: PADX, top: 6.5cm, {
    let num = s.at("number", default: "")
    if num != "" {
      text(size: 64pt, weight: 600, fill: if on-photo { rgb(255, 255, 255, 130) } else { p.fg2.transparentize(40%) })[#num]
      v(2pt)
    }
    text(size: 40pt, weight: 600, fill: tcol)[#s.at("title", default: "")]
    let sm = s.at("summary", default: "")
    if sm != "" { v(8pt); text(size: 16pt, fill: scol)[#sm] }
  })
  footer-row(p, n)
}

#let l-content(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  let tags = s.at("tags", default: ())
  if tags.len() > 0 { v(12pt); badge-row(p, tags) }
  v(14pt)
  let body = s.at("body", default: "")
  let bullets = s.at("bullets", default: ())
  if body != "" {
    set par(leading: 0.7em, spacing: 1.0em, justify: false)
    text(size: 15pt, fill: p.fg2, eval(body, mode: "markup"))
  }
  if bullets.len() > 0 {
    set par(leading: 0.6em)
    for b in bullets {
      grid(
        columns: (auto, 1fr), column-gutter: 10pt,
        text(size: 15pt, fill: purple)[▸],
        text(size: 15pt, fill: p.fg, eval(b, mode: "markup")),
      )
      v(7pt)
    }
  }
})

#let l-agenda(p, n, s) = chrome(p, n, s, {
  text(size: 30pt, weight: 600, fill: p.fg)[#s.at("title", default: "Agenda")]
  v(18pt)
  let items = s.at("items", default: ())
  let rows = ()
  for (i, it) in items.enumerate() {
    let chip = num-badge(p, [#{ if i + 1 < 10 { "0" } }#str(i + 1)])
    rows.push((chip, it))
  }
  grid(
    columns: (1fr, 1fr), column-gutter: 1.2cm, row-gutter: 0.9cm,
    ..rows.map(((chip, it)) => grid(
      columns: (auto, 1fr), column-gutter: 14pt, align: horizon,
      chip,
      [
        #text(size: 16pt, weight: 600, fill: p.fg)[#it.at("label", default: "")]
        #let note = it.at("note", default: "")
        #if note != "" [ \ #text(size: 12pt, fill: p.muted)[#note]]
      ],
    ))
  )
})

#let l-cards(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(18pt)
  let cards = s.at("cards", default: ())
  let gradient-cards = s.at("card_style", default: "glass") == "gradient"
  grid(
    columns: (1fr,) * calc.max(cards.len(), 1), column-gutter: 0.7cm, rows: (8.7cm,),
    ..cards.map(c => {
      let ic = c.at("icon", default: "")
      let im = c.at("image", default: "")
      let icon-cell = if ic == "" { [] } else if icon-names.contains(ic) {
        lucide(ic, p.icon-hex, 22pt)
      } else { text(size: 19pt, fill: p.fg)[#ic] }
      let head = grid(
        columns: (auto, 1fr), column-gutter: 10pt, align: horizon,
        icon-cell,
        text(size: 17pt, weight: 600, fill: p.fg)[#c.at("title", default: "")],
      )
      let body = text(size: 12.5pt, fill: p.fg2, eval(c.at("body", default: ""), mode: "markup"))
      // optional inline "learn more ↗" link (example.com product-card style)
      let lk = c.at("link", default: "")
      let link-line = if lk == "" { [] } else {
        v(10pt)
        grid(columns: (auto, auto), column-gutter: 7pt, align: horizon,
          text(size: 12.5pt, weight: 600, fill: purple)[#lk], btn-arrow("#2563EB", size: 13pt))
      }
      if im != "" {
        // image banner on top, content below — card clips to 18px radius
        block(fill: p.card-fill, stroke: 0.7pt + p.card-line, radius: 18pt, clip: true, width: 100%, height: 100%)[
          #image(im, width: 100%, height: 3.4cm, fit: "cover")
          #pad(x: 16pt, y: 16pt)[#head #v(8pt) #body #link-line]
        ]
      } else if gradient-cards {
        grad-card(p, { head; v(10pt); body; link-line })
      } else {
        glass(p, { head; v(10pt); body; link-line })
      }
    })
  )
})

#let l-split(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(16pt)
  let kind = s.at("right_kind", default: "text")
  let right = s.at("right", default: "")
  let right-block = if kind == "code" {
    glass(p, text(size: 11pt, font: "DejaVu Sans Mono", fill: p.fg)[#raw(right)])
  } else if kind == "image" and right != "" {
    if s.at("frame", default: false) {
      align(horizon, device-frame(right, h: 6.6cm))
    } else {
      block(radius: 18pt, clip: true, width: 100%, height: 100%, image(right, width: 100%, height: 100%, fit: "cover"))
    }
  } else {
    glass(p, text(size: 13.5pt, fill: p.fg2)[#right])
  }
  grid(
    columns: (1fr, 1fr), column-gutter: 0.9cm, rows: (7.8cm,),
    {
      let lb = s.at("left_body", default: "")
      if lb != "" { text(size: 14pt, fill: p.fg2, eval(lb, mode: "markup")); v(8pt) }
      for b in s.at("left_bullets", default: ()) {
        grid(columns: (auto, 1fr), column-gutter: 9pt,
          text(size: 14pt, fill: purple)[▸], text(size: 14pt, fill: p.fg, eval(b, mode: "markup")))
        v(6pt)
      }
    },
    right-block,
  )
})

#let l-stats(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(0.9cm)
  let stats = s.at("stats", default: ())
  grid(
    columns: (1fr,) * calc.max(stats.len(), 1), column-gutter: 0.6cm,
    ..stats.map(st => align(center, {
      grad-text(st.at("value", default: ""), size: 50pt, weight: 600)
      v(6pt)
      text(size: 15pt, weight: 600, fill: p.fg)[#st.at("label", default: "")]
      let note = st.at("note", default: "")
      if note != "" { v(3pt); linebreak(); text(size: 11.5pt, fill: p.muted)[#note] }
    }))
  )
})

// Metric dashboard: a row of glass cards, each a real metric — label, big
// gradient value, an optional signed trend, and an optional sparkline. Native
// vector (crisp at any zoom, editable in the pptx export), so a deck shows
// proper numbers instead of a screenshot mock.
#let l-dashboard(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(0.8cm)
  let cards = s.at("cards", default: ())
  let cols = calc.max(cards.len(), 1)
  grid(
    columns: (1fr,) * cols, column-gutter: 0.6cm, rows: (6.4cm,),
    ..cards.enumerate().map(ic => {
      let c = ic.at(1)
      glass(p, {
        text(size: 11pt, weight: 600, tracking: 1.2pt, fill: p.muted)[#upper(c.at("label", default: ""))]
        v(10pt)
        grad-text(c.at("value", default: ""), size: 34pt, weight: 600)
        let tr = c.at("trend", default: "")
        if tr != "" { linebreak(); v(2pt); trend-chip(p, tr) }
        let sp = c.at("spark", default: ())
        if type(sp) == array and sp.len() >= 2 {
          // push the sparkline to the card bottom; alternate the brand hues
          place(bottom + left, dx: 0pt, dy: 0pt, box(width: 100%,
            sparkline(sp, paint: if calc.rem(ic.at(0), 2) == 0 { purple } else { peach })))
        }
      }, inset: 16pt)
    })
  )
})

#let l-comparison(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(16pt)
  let left = s.at("left", default: (:))
  let right = s.at("right", default: (:))
  let hl = s.at("highlight", default: "right")
  let col(data, win, mark, markcolor) = {
    let inner = {
      text(size: 16pt, weight: 600, fill: p.fg)[#data.at("heading", default: "")]
      v(10pt)
      for pt in data.at("points", default: ()) {
        grid(columns: (auto, 1fr), column-gutter: 9pt,
          text(size: 14pt, fill: markcolor)[#mark], text(size: 13.5pt, fill: p.fg2)[#pt])
        v(7pt)
      }
    }
    if win { grad-card(p, inner) } else { glass(p, inner) }
  }
  grid(
    columns: (1fr, 1fr), column-gutter: 0.9cm, rows: (7.6cm,),
    col(left, hl == "left", "×", peach),
    col(right, hl == "right", "✓", purple),
  )
})

#let l-process(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(1.2cm)
  let steps = s.at("steps", default: ())
  let cnt = calc.max(steps.len(), 1)
  // band: connector line runs through the vertical centre of the badges
  block(width: 100%, height: 1.3cm, {
    place(left + horizon, line(length: 100%, stroke: 2.5pt + grad))
    grid(
      columns: (1fr,) * cnt, align: center + horizon,
      ..steps.enumerate().map(((i, st)) => num-badge(p, str(i + 1)))
    )
  })
  v(14pt)
  grid(
    columns: (1fr,) * cnt, align: center,
    ..steps.map(st => {
      text(size: 15pt, weight: 600, fill: p.fg)[#st.at("label", default: "")]
      let d = st.at("detail", default: "")
      if d != "" { v(4pt); linebreak(); text(size: 12pt, fill: p.muted)[#d] }
    })
  )
})

#let l-quote(p, n, s) = {
  let hero = s.at("style", default: "default") == "hero"
  let attr = [— #text(weight: 600)[#s.at("attribution", default: "")]#{
    let r = s.at("attribution_role", default: "")
    if r != "" [#text(fill: p.muted)[, #r]]
  }]
  if hero {
    // big, centred, white — the example.com hero-quote look (own chrome, like statement)
    place(top + left, dx: PADX, dy: 1.15cm, logo-img(p))
    place(top + right, dx: -PADX, dy: 1.3cm, box(width: 14cm, kicker(p, s)))
    place(center + horizon, box(width: 25cm, align(center, {
      set par(leading: 0.4em)
      text(size: 34pt, weight: 600, fill: p.fg)[“#s.at("quote", default: "")”]
      v(24pt)
      text(size: 15pt, fill: p.fg2)[#attr]
    })))
    footer-row(p, n)
  } else {
    chrome(p, n, s, {
      v(1.6cm)
      set par(leading: 0.5em)
      grad-text([“#s.at("quote", default: "")”], size: 30pt)
      v(22pt)
      text(size: 15pt, fill: p.fg)[#attr]
    })
  }
}

// `size` (small|medium|large|full) scales the picture as a FRACTION of the space
// that is actually free — the region `chrome` leaves between logo and footer —
// not as an absolute height. Fractions cannot overflow; the absolute heights this
// replaces (large = 12cm, full = 14.5cm) were up to ~1cm over that budget once a
// title and a caption were added, and typst silently pushed the overflow onto a
// blank extra slide instead of shrinking the image.
#let image-scale(s, default-key: "large") = (
  small: 0.45, medium: 0.68, large: 0.88, full: 1.0,
).at(s.at("size", default: default-key), default: 0.88)

// A single image on its own clean slide — centred, sized via `size`, NO browser
// chrome and NO text overlay (unlike full-bleed `l-media`). Optional `title`
// above and `caption` below sit in the flow, so nothing ever covers the image.
// Rows are (auto, 1fr, auto) inside a full-height box: title and caption take
// exactly what they need, the picture gets everything left over and never more,
// and `fit: "contain"` shows all of it, uncropped. Use this for a
// diagram/photo/chart that should read large and unobstructed.
#let l-image(p, n, s) = chrome(p, n, s, {
  let img = s.at("image", default: "")
  let title = s.at("title", default: "")
  let cap = s.at("caption", default: "")
  let framed = s.at("frame", default: false)
  let scale = image-scale(s)
  box(width: 100%, height: CH, grid(
    columns: (1fr,), rows: (auto, 1fr, auto), row-gutter: 0.45cm,
    if title == "" { [] } else { align(center, text(size: 26pt, weight: 600, fill: p.fg)[#title]) },
    align(center + horizon, if img == "" { [] } else if framed {
      // the frame adds its own title bar, so leave it room on top of the picture
      layout(sz => align(center, box(width: scale * 100%,
        device-frame(img, h: calc.max(sz.height * scale - 1.2cm, 1cm)))))
    } else {
      box(width: scale * 100%, height: scale * 100%,
        image(img, width: 100%, height: 100%, fit: "contain"))
    }),
    if cap == "" { [] } else { align(center, text(size: 13pt, fill: p.muted)[#cap]) },
  ))
})

// Image showcase. Text on a full-slide picture is the EXCEPTION here, not the
// rule: a picture big enough to fill a slide is usually a diagram, a chart or a
// screenshot, i.e. it already carries its own labels, and a headline dropped on
// top of those is unreadable (the `gallery` tiles put their captions in a strip
// BELOW the image for exactly this reason). So `media` picks the treatment:
//   • no title and no caption → the picture alone, edge to edge, nothing on it
//   • title/caption, no `overlay_position` → the clean `image` slide: picture as
//     large as it fits, title above it, caption below it, never over it
//   • `overlay_position` set → opt IN to the glass caption panel ON the picture
//     (bottom-left | bottom-right | center); for photos/artwork with empty space
//   • `size` other than "full" → the clean sized `image` slide, so "make the
//     picture smaller" does something on this layout too (it used to be ignored
//     unless `frame` was set, which is why no amount of asking ever shrank it)
//   • `frame: true` → screenshot inside the browser/device chrome
// A picture whose aspect is too far off 16:9 to crop is never full-bleed either:
// it falls back to the clean slide rather than losing a quarter of itself.
#let l-media(p, s, n) = {
  let img = s.at("image", default: "")
  let framed = s.at("frame", default: false)
  let title = s.at("title", default: "")
  let cap = s.at("caption", default: "")
  let overlay-at = s.at("overlay_position", default: "")
  let has-text = title != "" or cap != ""
  let white-logo = (logo: "assets/logo-light.svg", card-inner: anthracite, fg: white, muted: rgb(255, 255, 255, 220))
  if img != "" and framed {
    // framed screenshot, centred on the (theme) background — a clean product slide
    place(top + left, dx: PADX, dy: 1.15cm, logo-img(p))
    if title != "" { place(top + left, dx: PADX, dy: 2.7cm, text(size: 26pt, weight: 600, fill: p.fg)[#title]) }
    // `size` scales the framed screenshot (default keeps the original 9.4cm).
    let h = (small: 6cm, medium: 8cm, large: 9.4cm, full: 11cm).at(s.at("size", default: "large"), default: 9.4cm)
    place(center + horizon, dy: -0.2cm, box(width: 21cm, device-frame(img, h: h)))
    if cap != "" { place(center + horizon, dy: 5.7cm, text(size: 13pt, fill: p.muted)[#cap]) }
    footer-row(p, n)
  } else if img != "" and (s.at("size", default: "full") != "full" or (has-text and overlay-at == "")) {
    // asked to be smaller, or carrying text → the clean image slide, no overlay
    l-image(p, n, (..s, size: s.at("size", default: "full")))
  } else if img != "" {
    context if not croppable(img, SW / SH) {
      // too tall/square to fill a 16:9 slide without cutting it up
      l-image(p, n, (..s, size: "full"))
    } else {
      // edge-to-edge picture; logo and footer are the only marks on it
      place(top + left, image(img, width: SW, height: SH, fit: "cover"))
      if has-text {
        // opt-in glass caption panel. Nearly opaque, so it reads as a card
        // instead of ghosting whatever sits behind it.
        let (anchor, dx, dy) = if overlay-at == "bottom-right" {
          (bottom + right, -PADX, -1.6cm)
        } else if overlay-at == "center" {
          (center + horizon, 0cm, 0cm)
        } else {
          (bottom + left, PADX, -1.6cm)
        }
        place(anchor, dx: dx, dy: dy, box(width: 18cm, block(
          fill: anthracite.transparentize(6%), stroke: 0.7pt + rgb(255, 255, 255, 60), radius: 18pt, inset: 18pt,
        )[
          #text(size: 22pt, weight: 600, fill: white)[#title]
          #if cap != "" { v(4pt); text(size: 13pt, fill: rgb(255, 255, 255, 220))[#cap] }
        ]))
      }
      place(top + left, dx: PADX, dy: 1.3cm, logo-img(white-logo, h: 0.7cm))
      footer-row(white-logo, n, on-grad: true)
    }
  } else {
    // no image: a centred statement on the gradient, so the slide reads as intentional
    place(top + left, rect(width: SW, height: SH, fill: grad))
    place(top + left, dx: PADX, dy: 1.3cm, logo-img(white-logo, h: 0.7cm))
    place(center + horizon, box(width: 24cm, align(center, {
      text(size: 38pt, weight: 600, fill: white)[#title]
      if cap != "" { v(12pt); text(size: 17pt, fill: rgb(255, 255, 255, 230))[#cap] }
    })))
    footer-row(white-logo, n, on-grad: true)
  }
}

#let l-team(p, n, s) = chrome(p, n, s, {
  text(size: 30pt, weight: 600, fill: p.fg)[#s.at("title", default: "")]
  v(0.9cm)
  let ppl = s.at("people", default: ())
  grid(
    columns: (1fr,) * calc.max(ppl.len(), 1), column-gutter: 0.6cm,
    ..ppl.map(person => align(center, {
      avatar(p, person)
      v(10pt)
      text(size: 15pt, weight: 600, fill: p.fg)[#person.at("name", default: "")]
      v(2pt); linebreak()
      text(size: 12pt, fill: p.muted)[#person.at("role", default: "")]
    }))
  )
})

#let l-table(p, n, s) = chrome(p, n, s, {
  text(size: 30pt, weight: 600, fill: p.fg)[#s.at("title", default: "")]
  v(16pt)
  let cols = s.at("columns", default: ())
  let rows = s.at("rows", default: ())
  let ncol = calc.max(cols.len(), 1)
  table(
    columns: (1fr,) * ncol,
    stroke: none,
    inset: 11pt,
    fill: (col, row) => if row == 0 { purple } else if calc.even(row) { p.zebra } else { none },
    table.header(..cols.map(c => text(size: 13pt, weight: 600, fill: white)[#c])),
    ..rows.flatten().enumerate().map(((i, cell)) => text(size: 13pt, fill: p.fg)[#cell]),
  )
})

#let l-diagram(p, n, s) = chrome(p, n, s, {
  text(size: 30pt, weight: 600, fill: p.fg)[#s.at("title", default: "")]
  v(1.0cm)
  let nodes = s.at("nodes", default: ())
  // interleave nodes with gradient arrows
  let cells = ()
  let tracks = ()
  for (i, nd) in nodes.enumerate() {
    cells.push(box(width: 4.4cm, height: 2.4cm, glass(p, align(center + horizon, text(size: 14pt, weight: 600, fill: p.fg)[#nd.at("label", default: "")]), inset: 8pt)))
    tracks.push(auto)
    if i < nodes.len() - 1 {
      cells.push(align(horizon, text(size: 22pt, fill: purple)[→]))
      tracks.push(auto)
    }
  }
  align(center, grid(columns: tracks, column-gutter: 0.5cm, align: horizon, ..cells))
  let lg = s.at("legend", default: "")
  if lg != "" { v(0.8cm); align(center, text(size: 12pt, fill: p.muted)[#lg]) }
})

#let l-closing(p, n, s) = {
  let white-logo = (logo: "assets/logo-light.svg")
  // no bg_image → a full brand-gradient background so the CTA button pops
  let bg = resolve-bg(s, "")
  if bg != "" { bg-scrim(bg, scrim: 30%) } else { place(top + left, rect(width: SW, height: SH, fill: grad)) }
  place(top + left, dx: PADX, dy: 1.3cm, logo-img(white-logo, h: 0.7cm))
  pad(left: PADX, right: PADX, top: 5.4cm, {
    text(size: 44pt, weight: 600, fill: white)[#s.at("headline", default: "Let's build it together")]
    let sl = s.at("subline", default: "")
    if sl != "" { v(12pt); text(size: 18pt, fill: rgb(255, 255, 255, 220))[#sl] }
    let btns = s.at("buttons", default: ())
    let cta = s.at("cta_label", default: "")
    if btns.len() > 0 { v(22pt); button-row(btns, default-style: "primary") }
    else if cta != "" { v(22pt); cta-button(cta) }
    let parts = ()
    let nm = s.at("contact_name", default: "")
    let em = s.at("contact_email", default: "")
    if nm != "" { parts.push(nm) }
    if em != "" { parts.push(em) }
    if parts.len() > 0 {
      v(26pt)
      text(size: 13pt, fill: white)[#parts.join("   ·   ")]
    }
  })
  footer-row(white-logo, n, on-grad: true)
}

// big centred statement on a solid (theme) background
#let l-statement(p, n, s) = {
  let bg = resolve-bg(s, "")
  let on-photo = bg != ""
  bg-scrim(bg, scrim: 42%)
  place(top + left, dx: PADX, dy: 1.15cm, logo-img(if on-photo { (logo: "assets/logo-light.svg") } else { p }))
  // the bundled grainients are dark → force white text regardless of theme
  let tcol = if on-photo { white } else { p.fg }
  let scol = if on-photo { rgb(255, 255, 255, 230) } else { p.fg2 }
  place(center + horizon, box(width: 26cm, align(center, {
    let eb = s.at("eyebrow", default: "")
    if eb != "" { text(size: 12pt, weight: 600, tracking: 2pt, fill: if on-photo { rgb(255, 255, 255, 235) } else { purple })[#upper(eb)]; v(14pt) }
    text(size: 40pt, weight: 600, fill: tcol)[#s.at("title", default: "")]
    let sub = s.at("subtitle", default: "")
    if sub != "" { v(14pt); text(size: 18pt, fill: scol)[#sub] }
  })))
  footer-row(p, n)
}

// one hero metric + supporting copy
#let l-bignumber(p, n, s) = chrome(p, n, s, {
  grid(
    columns: (auto, 1fr), column-gutter: 1.4cm, align: horizon, rows: (7.5cm,),
    align(horizon, grad-text(s.at("value", default: ""), size: 110pt, weight: 600)),
    align(horizon, {
      text(size: 24pt, weight: 600, fill: p.fg)[#s.at("title", default: "")]
      let body = s.at("body", default: "")
      if body != "" { v(10pt); text(size: 15pt, fill: p.fg2)[#body] }
    }),
  )
})

// "trusted by" logo / name wall (glass chips)
#let l-logos(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(0.9cm)
  let names = s.at("logos", default: ())
  let per-row = s.at("per_row", default: 4)
  grid(
    columns: (1fr,) * per-row, column-gutter: 0.6cm, row-gutter: 0.6cm,
    ..names.map(nm => box(height: 2.2cm, glass(p, align(center + horizon, text(size: 17pt, weight: 600, fill: p.fg2)[#nm]))))
  )
})

// image gallery / grid (gradient-tile fallback when no image given)
#let l-gallery(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(0.8cm)
  let items = s.at("items", default: ())
  let per-row = s.at("per_row", default: 3)
  grid(
    columns: (1fr,) * per-row, column-gutter: 0.5cm, row-gutter: 0.5cm,
    ..items.map(it => {
      let img = it.at("image", default: "")
      let cap = it.at("caption", default: "")
      // solid caption strip BELOW the image (not overlaid) so text stays legible
      // regardless of what the picture shows underneath. A fixed-row grid keeps
      // the image and the caption strip strictly separated.
      let strip = if p.dark { rgb("#26262A") } else { rgb("#ECECEE") }
      let cap-h = if cap != "" { 1.0cm } else { 0cm }
      let media = if img != "" { image(img, width: 100%, height: 100%, fit: "cover") } else { rect(width: 100%, height: 100%, fill: grad) }
      block(radius: 18pt, clip: true, width: 100%, height: 4.6cm, fill: strip,
        grid(
          rows: (1fr, cap-h), row-gutter: 0pt,
          box(width: 100%, height: 100%, clip: true, media),
          align(left + horizon, box(inset: (left: 12pt), text(size: 11.5pt, weight: 600, fill: p.fg)[#cap])),
        ))
    })
  )
})

// vertical roadmap / timeline
#let l-timeline(p, n, s) = chrome(p, n, s, {
  title-block(p, s)
  v(0.7cm)
  let ms = s.at("milestones", default: ())
  for (i, m) in ms.enumerate() {
    grid(
      columns: (auto, 1fr), column-gutter: 16pt, align: (center, left),
      {
        num-badge(p, str(i + 1), d: 1.0cm)
        if i < ms.len() - 1 { place(center, dx: 0pt, dy: 0pt, line(start: (0.5cm, 1.0cm), end: (0.5cm, 1.9cm), stroke: 2pt + grad)) }
      },
      {
        let dt = m.at("date", default: "")
        text(size: 16pt, weight: 600, fill: p.fg)[#m.at("label", default: "")]
        if dt != "" { h(10pt); text(size: 12pt, weight: 600, fill: purple)[#dt] }
        let d = m.at("detail", default: "")
        if d != "" { linebreak(); text(size: 13pt, fill: p.fg2)[#d] }
      },
    )
    v(0.45cm)
  }
})

// full-width gradient callout band on a dark slide (example.com "Our Mission" /
// "Are you ready for the next step?" sections). Buttons default to light+outline.
#let l-cta-panel(p, n, s) = {
  place(top + left, dx: PADX, dy: 1.15cm, logo-img(p))
  let m = 1.5cm
  place(center + horizon, box(width: SW - 2 * m, fill: grad, radius: 28pt, inset: (x: 2.2cm, y: 1.7cm), align(center, {
    let k = s.at("kicker", default: "")
    if k != "" { text(size: 12pt, weight: 600, tracking: 1.5pt, fill: rgb(255, 255, 255, 235))[#upper(k)]; v(12pt) }
    text(size: 32pt, weight: 600, fill: white)[#s.at("title", default: "")]
    let body = s.at("body", default: "")
    if body != "" { v(12pt); text(size: 16pt, fill: rgb(255, 255, 255, 235))[#body] }
    let btns = s.at("buttons", default: ())
    let cta = s.at("cta_label", default: "")
    if btns.len() > 0 { v(22pt); button-row(btns, default-style: "light") }
    else if cta != "" { v(22pt); btn(cta, style: "light") }
  })))
  footer-row(p, n)
}

// ============================================================================
// RENDER
// ============================================================================
#set text(font: "Urbanist", fill: white)
#set page(width: SW, height: SH, margin: 0pt)

// Brand styling for prose written as native Typst markup inside slide bodies
// (content/split `body`+`bullets`, card bodies). The model writes plain Typst —
// *bold*, _italic_, `- `/`+ ` lists, #quote — and these rules colour Typst's own
// elements in the brand. They only affect eval'd body markup: the layouts build
// their titles/tables/bullets with text()/table()/grid(), not these elements.
#let body-heading-sizes = (20pt, 17pt, 15pt)
#show heading: it => {
  set text(fill: purple, weight: 600, size: body-heading-sizes.at(calc.min(it.level, 3) - 1))
  block(above: 8pt, below: 5pt, it.body)
}
#show link: it => text(fill: purple, underline(it))
#show quote.where(block: true): it => block(
  width: 100%, above: 6pt, below: 6pt, inset: (left: 12pt, top: 3pt, bottom: 3pt),
  stroke: (left: 3pt + purple), it.body,
)
#set enum(numbering: n => text(fill: purple, weight: 600)[#n.])
#set list(marker: (text(fill: purple)[▸], text(fill: purple)[‣], text(fill: purple)[·]))

// Deck-wide theme keeps one consistent look; a slide may still override via
// its own `theme`, but the default avoids mixing dark + light in one deck.
// Precedence: --input theme=… overrides the deck's theme, which defaults dark.
#let deck-theme = sys.inputs.at("theme", default: deck.at("theme", default: "dark"))

#for (i, s) in slides.enumerate() {
  let n = i + 1
  let theme = s.at("theme", default: deck-theme)
  let p = palette(theme)
  let lay = s.at("layout", default: "content")
  page(fill: p.bg, {
    if lay == "cover" { l-cover(p, s) } else if lay == "section" { l-section(p, n, s) } else if lay == "agenda" { l-agenda(p, n, s) } else if lay == "cards" { l-cards(p, n, s) } else if lay == "split" { l-split(p, n, s) } else if lay == "stats" { l-stats(p, n, s) } else if lay == "dashboard" { l-dashboard(p, n, s) } else if lay == "comparison" { l-comparison(p, n, s) } else if lay == "process" { l-process(p, n, s) } else if lay == "quote" { l-quote(p, n, s) } else if lay == "media" { l-media(p, s, n) } else if lay == "image" { l-image(p, n, s) } else if lay == "team" { l-team(p, n, s) } else if lay == "table" { l-table(p, n, s) } else if lay == "diagram" { l-diagram(p, n, s) } else if lay == "statement" { l-statement(p, n, s) } else if lay == "bignumber" { l-bignumber(p, n, s) } else if lay == "logos" { l-logos(p, n, s) } else if lay == "gallery" { l-gallery(p, n, s) } else if lay == "timeline" { l-timeline(p, n, s) } else if lay == "cta-panel" { l-cta-panel(p, n, s) } else if lay == "closing" { l-closing(p, n, s) } else { l-content(p, n, s) }
  })
}
