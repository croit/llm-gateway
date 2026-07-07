# STATUS: llm-generated, unreviewed — pending native-speaker QA

backends-page-title = Восходящие бэкенды — LLM Gateway
backends-heading = Восходящие бэкенды
backends-description-prefix = Живой обзор настроенных восходящих пулов — состояние, текущая нагрузка относительно лимита каждого бэкенда и модели, которые каждый из них сейчас предоставляет. Только для чтения: маршрутизация полностью зависит от того, что бэкенды сообщают через свой
backends-description-suffix = проверочный запрос.
backends-summary = { $total } бэкендов · { $healthy } исправны · { $down } недоступны
backends-unknown-fallback-prefix = Резерв для неизвестной модели —
backends-empty-prefix = Восходящие пулы не настроены. Добавьте блок
backends-empty-suffix = в gateway.toml и перезапустите.

backends-fallback-offline-title = fallback_offline: используется, когда все бэкенды известной модели в этом пуле недоступны
backends-fallback-offline-badge = офлайн ↩ { $model }
backends-pool-empty = В этом пуле нет бэкендов.

backends-status-down = недоступен
backends-status-saturated = перегружен
backends-status-up = активен

backends-inflight-label = в обработке { $load }
backends-activity-summary = 15м { $m15 } · 30м { $m30 } · 60м { $m60 }
backends-no-models = модели не заявлены
backends-aliases-label = алиасы:

backends-alias-target-title = алиас → { $target }
backends-alias-disabled-label = { $name } (отключён)
backends-alias-disabled-title = простой алиас отключён — этот бэкенд обслуживает несколько моделей; укажите явную цель (форма сопоставления)
backends-alias-bare-title = алиас → модель этого бэкенда
