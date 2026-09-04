# STATUS: llm-generated, unreviewed — pending native-speaker QA

admin-tokens-page-title = API-токены
admin-tokens-heading = API-токены
admin-tokens-blurb = Все API-токены этой установки и их владельцы. Сам токен никогда не показывается — хранится только SHA-256, поэтому восстановить его здесь нельзя. Квоты задаются для каждого токена на странице лимитов. Список разрешённых моделей состоит из двух независимых половин — списка владельца на его странице токенов и вашего ниже — и токен может использовать только модели, входящие в оба списка, поэтому каждая сторона может лишь сузить его.
admin-tokens-none = API-токены ещё не создавались.
admin-tokens-count = токенов: { $count }
admin-tokens-col-name = Токен
admin-tokens-col-owner = Владелец
admin-tokens-col-state = Состояние
admin-tokens-col-dates = Создан / использован / истекает
admin-tokens-col-scope = Модели и квота
admin-tokens-badge-expired = истёк
admin-tokens-models-summary-all = Модели: все (без ограничения оператора)
admin-tokens-models-summary-restricted = Модели: оператор разрешает { $count }
admin-tokens-models-help = Ограничение оператора для этого токена, отдельное от ограничения владельца. Токен может использовать только модели, входящие в оба списка: отметка здесь не даёт доступ к модели, исключённой владельцем, а владелец не может вернуть ту, которую вы убрали.
admin-tokens-models-restrict-label = Ограничить этот токен определёнными моделями
admin-tokens-models-saved-toast = Ограничение оператора задано: { $count } моделей.
admin-tokens-models-cleared-toast = Ограничение оператора снято.
