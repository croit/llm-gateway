# Deployment setup wizard (/setup). Seen once per installation, by an
# operator, before any account exists.

setup-step-1-of-2 = Step 1 of 2
setup-provider-heading = Connect your identity provider
setup-provider-intro = This gateway has no accounts of its own — people sign in through your OIDC provider. Enter it below and we will try a real sign-in before saving anything.

setup-field-public-url = Public URL of this gateway
setup-field-public-url-help = The address your users will open. It must match exactly, including https, because sign-in redirects are built from it.

setup-redirect-uri-heading = Whitelist this redirect URI in your provider
setup-redirect-uri-help = Add it to the client's allowed redirect URIs before continuing. A provider that does not recognise it will refuse the sign-in.

setup-field-issuer = Issuer URL
setup-field-issuer-help = Copy it exactly as your provider reports it — the trailing slash matters. Keycloak omits it, Authentik expects it.

setup-field-client-id = Client ID
setup-field-client-secret = Client secret

setup-field-scopes = Scopes
setup-field-scopes-help = Space-separated. openid is always requested. Keep the one that carries group membership.

setup-field-roles-claim = Group claim
setup-field-roles-claim-help = Which claim lists a user's groups. Unsure? Leave it and pick from your own token on the next screen.

setup-test-button = Sign in to test
setup-test-button-help = Nothing is saved yet. You will come back here after signing in.

setup-step-2-of-2 = Step 2 of 2
setup-admin-heading = Choose who administers this gateway
setup-login-worked = Sign-in worked. Your provider identified you as:
setup-admin-intro = Below is what your provider actually said about you. Pick the group that should grant full administrative access — everyone else who signs in gets an ordinary account.
setup-no-claims = Your provider sent no group-like claims. Type the claim and value by hand below, or add a groups scope to the client and try again.

setup-or-manual = or enter it manually
setup-manual-claim = Claim
setup-manual-value = Value
setup-manual-help = Use this if the group that should be admin is not one you are in yourself. A value entered here overrides the selection above.

setup-finish-button = Finish setup
setup-back-button = Back to provider settings
setup-show-token = Show everything the provider sent
