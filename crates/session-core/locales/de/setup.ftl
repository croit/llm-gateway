# Einrichtungsassistent (/setup). Wird einmal pro Installation von einem
# Betreiber gesehen, bevor irgendein Konto existiert.

setup-step-1-of-2 = Schritt 1 von 2
setup-provider-heading = Identity Provider verbinden
setup-provider-intro = Dieses Gateway hat keine eigenen Konten — angemeldet wird über Ihren OIDC-Provider. Tragen Sie ihn unten ein; wir führen eine echte Anmeldung durch, bevor irgendetwas gespeichert wird.

setup-field-public-url = Öffentliche URL dieses Gateways
setup-field-public-url-help = Die Adresse, die Ihre Nutzer aufrufen. Sie muss exakt stimmen, inklusive https — die Anmelde-Weiterleitungen werden daraus gebildet.

setup-redirect-uri-heading = Diese Redirect-URI im Provider freigeben
setup-redirect-uri-help = Tragen Sie sie vor dem Fortfahren in die erlaubten Redirect-URIs des Clients ein. Ein Provider, der sie nicht kennt, verweigert die Anmeldung.

setup-field-issuer = Issuer-URL
setup-field-issuer-help = Exakt so übernehmen, wie Ihr Provider sie meldet — der abschließende Schrägstrich zählt. Keycloak lässt ihn weg, Authentik erwartet ihn.

setup-field-client-id = Client-ID
setup-field-client-secret = Client Secret

setup-field-scopes = Scopes
setup-field-scopes-help = Durch Leerzeichen getrennt. openid wird immer angefragt. Behalten Sie den Scope, der die Gruppenzugehörigkeit liefert.

setup-field-roles-claim = Gruppen-Claim
setup-field-roles-claim-help = Welcher Claim die Gruppen einer Person auflistet. Unsicher? Einfach stehen lassen und im nächsten Schritt aus Ihrem eigenen Token auswählen.

setup-test-button = Zum Testen anmelden
setup-test-button-help = Es wird noch nichts gespeichert. Nach der Anmeldung landen Sie wieder hier.

setup-step-2-of-2 = Schritt 2 von 2
setup-admin-heading = Festlegen, wer dieses Gateway administriert
setup-login-worked = Die Anmeldung hat funktioniert. Ihr Provider hat Sie identifiziert als:
setup-admin-intro = Unten steht, was Ihr Provider tatsächlich über Sie ausgesagt hat. Wählen Sie die Gruppe, die volle Administrationsrechte erhalten soll — alle anderen Anmeldungen bekommen ein normales Konto.
setup-no-claims = Ihr Provider hat keine gruppenartigen Claims geliefert. Tragen Sie Claim und Wert unten von Hand ein, oder ergänzen Sie einen groups-Scope am Client und versuchen Sie es erneut.

setup-or-manual = oder manuell eintragen
setup-manual-claim = Claim
setup-manual-value = Wert
setup-manual-help = Nutzen Sie das, wenn die Admin-Gruppe eine ist, in der Sie selbst nicht sind. Ein hier eingetragener Wert hat Vorrang vor der Auswahl oben.

setup-finish-button = Einrichtung abschließen
setup-back-button = Zurück zu den Provider-Einstellungen
setup-show-token = Alles anzeigen, was der Provider gesendet hat
