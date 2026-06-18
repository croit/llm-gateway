// Example corporate letter — DIN-5008-style layout, neutral sample branding.
// This is a generic, data-free SAMPLE template shipped with the gateway to
// demonstrate the `[typst]` feature. Replace the company data, colours, and
// logo below with your own, or point `[typst].templates_dir` at your own
// templates directory (see docs / gateway.example.toml).
//
// The model supplies CONTENT + two switches only; all company / legal / bank
// data is baked in here per entity, so the model never needs to know IBANs,
// register numbers, etc.:
//   entity   = "de" → Example GmbH  |  "us" → Example Corp Inc
//   language = "en" (default, company language) | "de"
// Values arrive as strings on `sys.inputs`.

#let inputs = sys.inputs

// ---- Brand tokens (sample palette — swap for your own) ---------------------
#let c-primary = rgb("#2563EB")
#let c-accent = rgb("#0EA5E9")
#let ink = rgb("#0F172A")
#let muted = rgb("#64748B")
#let brand = gradient.linear(c-primary, c-accent)

// ---- Switches --------------------------------------------------------------
// Example Corp Inc (US entity) is ALWAYS English; only Example GmbH honours an
// optional `language = "de"` (default English).
#let ekey = inputs.at("entity", default: "de")
#let lang = if ekey == "us" { "en" } else { inputs.at("language", default: "en") }
// Example GmbH register block — German vs English wording.
#let de-company = if lang == "de" {
  ("Amtsgericht Musterstadt, HRB 000000", "USt-IdNr. DE000000000", "GF: Jane Doe, John Doe")
} else {
  ("Amtsgericht Musterstadt, HRB 000000", "VAT ID DE000000000", "Directors: Jane Doe, John Doe")
}
// Footer column widths differ by entity: Example GmbH needs a wide Company
// column (long register line, short IBAN); Example Corp Inc the reverse (short
// directors line, long US bank name + routing/account).
#let footer-cols = if ekey == "us" {
  (1fr, 0.85fr, 1.05fr, 1.6fr)
} else {
  (0.95fr, 0.95fr, 1.6fr, 1.1fr)
}

// ---- Company data per entity (sample — replace with your own) --------------
#let entities = (
  de: (
    place: "Berlin",
    address: ("Example GmbH", "Musterstraße 1", "10115 Berlin"),
    company: de-company,
    contact: ("Tel. +49 30 0000000", "info@example.com", "www.example.com"),
    bank: ("Example Bank", "IBAN DE00 0000 0000 0000 0000 00", "BIC EXAMPLEXXX"),
  ),
  us: (
    place: "Anytown, CA",
    address: ("Example Corp Inc", "123 Example Avenue", "Suite 100", "Anytown, CA 94000, USA"),
    company: ("Directors:", "Jane Doe, John Doe"),
    contact: ("info@example.com", "www.example.com"),
    bank: ("Example Bank N.A.", "Routing 000000000", "Acct 0000000000", "SWIFT EXAMPUS00"),
  ),
)
#let co = entities.at(ekey, default: entities.de)

// ---- Localised fixed strings -----------------------------------------------
#let L = if lang == "de" {
  (
    closing: "Mit freundlichen Grüßen",
    addr: "Anschrift", contact: "Kontakt", company: "Handelsregister", bank: "Bankverbindung",
    months: ("Januar", "Februar", "März", "April", "Mai", "Juni", "Juli", "August", "September", "Oktober", "November", "Dezember"),
  )
} else {
  (
    closing: "Sincerely,",
    addr: "Address", contact: "Contact", company: "Company", bank: "Bank",
    months: ("January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"),
  )
}
#let today = datetime.today()
#let month-name = L.months.at(today.month() - 1)
#let date-str = if lang == "de" {
  [#today.day(). #month-name #today.year()]
} else {
  [#month-name #today.day(), #today.year()]
}

// ---- Page (structured 4-column footer; safe print clearance) ---------------
#set page(
  paper: "a4",
  margin: (left: 25mm, right: 20mm, top: 18mm, bottom: 32mm),
  // `footer-descent` small so the footer sits high in the bottom margin,
  // leaving ~14mm clearance to the sheet edge — inside any printer's area.
  footer: [
    #rect(width: 100%, height: 1.1pt, fill: brand, stroke: none)
    #v(4pt)
    #set text(size: 7pt, fill: muted)
    #set par(leading: 0.5em, justify: false)
    #let fcol(label, lines) = [
      #text(weight: "semibold", fill: c-primary)[#label]#linebreak()#lines.map(l => [#l]).join(linebreak())
    ]
    #grid(
      columns: footer-cols,
      column-gutter: 5mm,
      fcol(L.addr, co.address),
      fcol(L.contact, co.contact),
      fcol(L.company, co.company),
      fcol(L.bank, co.bank),
    )
  ],
  footer-descent: 3mm,
)
#set text(font: "Urbanist", size: 11pt, fill: ink, lang: lang)
#set par(justify: true, leading: 0.7em, spacing: 1.15em)

// ---- DIN 5008 fold + hole marks (~8mm from the sheet edge) -----------------
#let mark(y, len) = place(top + left, dx: -17mm, dy: y, line(length: len, stroke: 0.4pt + muted))
#mark(87mm - 18mm, 4mm) // upper fold
#mark(148.5mm - 18mm, 5mm) // hole punch
#mark(192mm - 18mm, 4mm) // lower fold

// ---- Sender (the person the letter is FROM; model-filled) ------------------
// Default to the current user, UNLESS the request asks to write on someone
// else's behalf (e.g. an assistant drafting for a manager).
#let s-name = inputs.at("sender_name", default: "")
#let s-title = inputs.at("sender_title", default: "")
#let s-email = inputs.at("sender_email", default: "")
#let s-phone = inputs.at("sender_phone", default: "")

// ---- Letterhead: logo (left) + sender block (right) ------------------------
#let sender-block = align(right, {
  set text(size: 8pt, fill: muted)
  set par(leading: 0.5em, justify: false)
  if s-name != "" { text(fill: ink, weight: "medium")[#s-name]; linebreak() }
  if s-title != "" { [#s-title]; linebreak() }
  if s-email != "" { [#s-email]; linebreak() }
  if s-phone != "" { [#s-phone] }
})
#grid(
  columns: (1fr, auto),
  align: (left + top, right + top),
  column-gutter: 8mm,
  image("assets/logo.svg", height: 9mm),
  sender-block,
)
#v(6pt)
#rect(width: 100%, height: 2pt, fill: brand, stroke: none)

#v(13mm)

// ---- Address field (return line + recipient) -------------------------------
#text(size: 7pt, fill: muted)[#co.address.join(" · ")]
#v(2.5mm)
#text(size: 11pt)[
  #inputs.recipient_name \
  #for line in inputs.recipient_address.split("\n") [#line \ ]
]

#v(9mm)

// ---- Date, right-aligned ---------------------------------------------------
#align(right)[#text(size: 10pt)[#co.place, #date-str]]

#v(5mm)

// ---- Subject, brand colour -------------------------------------------------
#text(weight: "semibold", size: 11.5pt, fill: c-primary)[#inputs.subject]

#v(4mm)

// ---- Body: split on blank lines into real paragraphs -----------------------
// Defensive closing de-dup: models routinely append a sign-off ("Best regards,
// \n Jane Doe") to the body even when told not to. We own the closing +
// signature below, so strip any trailing paragraph that is purely a known
// closing phrase and/or the sender's name/title — else the letter shows the
// closing (and name) twice and the extra lines push the real signature off
// the page.
#let signoffs = (
  "sincerely", "best regards", "kind regards", "warm regards", "best wishes",
  "yours sincerely", "yours faithfully", "yours truly", "regards", "best",
  "many thanks", "respectfully", "thank you",
  "mit freundlichen grüßen", "mit freundlichem gruß", "mit besten grüßen",
  "freundliche grüße", "beste grüße", "viele grüße", "herzliche grüße",
)
#let is-signoff(para) = {
  let lines = para.split("\n").map(l => l.trim()).filter(l => l != "")
  lines.len() > 0 and lines.all(l => {
    let key = lower(l.trim(".").trim(","))
    signoffs.contains(key) or l == s-name or l == s-title
  })
}
#let paras = inputs.body.split("\n\n")
#while paras.len() > 1 and is-signoff(paras.last()) {
  paras = paras.slice(0, -1)
}
// Safety net: guarantee an opening salutation even when the model forgets one
// (the manifest requires it, but compliance isn't 100%). A salutation the model
// did write is kept as-is; otherwise prepend a neutral, language-appropriate one
// so the letter never starts mid-sentence.
#let salut-markers = (
  "sehr geehrt", "guten tag", "liebe", "lieber", "hallo", "werte",
  "dear ", "hello", "hi ", "good ", "to whom", "greetings",
)
#let opener = if paras.len() > 0 {
  lower(paras.first().split("\n").first().trim())
} else {
  ""
}
#if not salut-markers.any(m => opener.starts-with(m)) {
  let fallback = if lang == "de" { "Sehr geehrte Damen und Herren," } else { "Dear Sir or Madam," }
  paras = (fallback,) + paras
}
#for block in paras {
  block.replace("\n", " ")
  parbreak()
}

#v(5mm)

// ---- Closing + signature ---------------------------------------------------
#L.closing

#v(16mm)

#text(weight: "medium")[#s-name]
#if s-title != "" [ \ #text(size: 9.5pt, fill: muted)[#s-title]]
