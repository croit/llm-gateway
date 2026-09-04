# STATUS: llm-generated, unreviewed — pending native-speaker QA

admin-tokens-page-title = API-Tokens
admin-tokens-heading = API-Tokens
admin-tokens-blurb = Alle API-Tokens dieser Installation und ihre Besitzer. Das Token selbst wird nie angezeigt — gespeichert ist nur ein SHA-256-Hash, es lässt sich hier also nicht wiederherstellen. Kontingente werden pro Token auf der Limits-Seite gesetzt. Die Modell-Freigabeliste besteht aus zwei unabhängigen Hälften — der des Besitzers auf seiner Token-Seite und Ihrer unten — und das Token darf nur Modelle verwenden, die auf beiden stehen; jede Seite kann also nur einschränken.
admin-tokens-none = Es wurden noch keine API-Tokens erstellt.
admin-tokens-count = { $count } Token(s)
admin-tokens-col-name = Token
admin-tokens-col-owner = Besitzer
admin-tokens-col-state = Status
admin-tokens-col-dates = Erstellt / verwendet / läuft ab
admin-tokens-col-scope = Modelle & Kontingent
admin-tokens-badge-expired = abgelaufen
admin-tokens-models-summary-all = Modelle: alle (keine Betreiber-Beschränkung)
admin-tokens-models-summary-restricted = Modelle: Betreiber erlaubt { $count }
admin-tokens-models-help = Eine Betreiber-Beschränkung für dieses Token, getrennt von der des Besitzers. Das Token darf nur Modelle verwenden, die auf beiden Listen stehen — ein Haken hier kann also kein Modell freigeben, das der Besitzer ausgeschlossen hat, und der Besitzer kann keines wieder freigeben, das Sie entfernen.
admin-tokens-models-restrict-label = Dieses Token auf bestimmte Modelle beschränken
admin-tokens-models-saved-toast = Betreiber-Beschränkung gesetzt: { $count } Modelle.
admin-tokens-models-cleared-toast = Betreiber-Beschränkung entfernt.
