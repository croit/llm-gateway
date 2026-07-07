# Strings owned by `gateway/src/rama_server/pages/admin_users.rs` — the
# admin user roster, the impersonate button per row, and the recent
# impersonation audit trail shown on `/admin/users`.

admin-users-page-title = Users — LLM Gateway
admin-users-heading = Users
admin-users-desc-allowed-prefix = Everyone who has signed in to this gateway, with their identity-provider groups and the gateway roles those map to.
admin-users-desc-allowed-suffix = starts a session that behaves exactly as that user — useful for reproducing what they see. Every impersonation is logged below.
admin-users-desc-disabled-prefix = Everyone who has signed in to this gateway, with their identity-provider groups and the gateway roles those map to. Impersonation is
admin-users-desc-disabled-suffix = on this gateway (`allow_impersonation = false`).
admin-users-disabled-label = disabled
admin-users-impersonate-button = Impersonate

admin-users-col-user = User
admin-users-col-oidc-groups = OIDC groups
admin-users-col-gateway-roles = Gateway roles
admin-users-col-joined = Joined
admin-users-col-action = Action

admin-users-you-badge = you
admin-users-no-oidc-groups = none
admin-users-no-gateway-roles = none granted

admin-users-audit-heading = Recent impersonation activity
admin-users-audit-empty = No impersonations recorded yet.
admin-users-audit-col-when = When
admin-users-audit-col-action = Action
admin-users-audit-col-admin = Admin
admin-users-audit-col-target = Target
