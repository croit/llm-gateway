# STATUS: llm-generated, unreviewed — pending native-speaker QA

skills-error-not-configured = Скиллы не настроены (параметр [skills] dir не задан).
skills-error-no-file = Файл не был загружен — выберите архив .skill.
skills-error-install-failed = Не удалось установить скилл: { $error }
skills-error-bad-delete-request = Некорректный запрос на удаление: { $error }
skills-error-delete-failed = Не удалось удалить скилл: { $error }
skills-page-title = Skills — LLM Gateway

skills-heading = Скиллы
skills-intro-part1 = Инструкции, установленные оператором, которые чат-модель подгружает по требованию через инструмент
skills-intro-part2 = предназначенный для этого. Загрузите архив
skills-intro-part3 = ниже — он доступен сразу, без перезапуска.
skills-empty-loaded = Скиллы пока не загружены. Загрузите архив .skill, чтобы добавить один.
skills-empty-not-configured = Скиллы не настроены. Укажите [skills] dir в конфигурации шлюза и перезапустите его, чтобы включить их.

skills-upload-heading = Добавить скилл
skills-upload-button = Загрузить .skill
skills-loaded-heading = Загруженные скиллы
skills-none-yet = Пока нет
skills-source-prefix = Источник:

skills-download-title = Скачать этот скилл как архив .skill
skills-download-button = Скачать
skills-delete-title = Удалить этот скилл
skills-delete-button = Удалить
skills-granted-to-heading = Доступ предоставлен
skills-granted-config-title = Предоставлено в конфигурации шлюза ([[roles]].skills)
skills-choose-access-title = Выберите, каким ролям разрешено использовать этот скилл
skills-no-grants-warning = ни одна роль не предоставляет доступ — настроить доступ
skills-edit-access-title = Изменить, каким ролям разрешено использовать этот скилл
skills-edit-access-button = Изменить доступ
skills-files-heading = Файлы
skills-files-count = { $count } в комплекте
skills-description-heading = Описание

skills-grant-dialog-heading = Кто может использовать этот скилл?
skills-grant-dialog-desc-part1 = Выберите роли, которым разрешено загружать этот скилл:
skills-grant-dialog-desc-part2 = . Доступ получит каждый, у кого выбрана роль.
skills-grant-dialog-no-roles-part1 = В конфигурации шлюза не определено ни одной роли. Добавьте записи
skills-grant-dialog-no-roles-part2 = прежде чем предоставлять доступ.
skills-cancel-button = Отмена
skills-save-access-button = Сохранить доступ

skills-from-config-badge = из конфигурации

skills-error-no-dir-access = Нет доступа к каталогу навыков — проверьте, что он существует и шлюз может читать и записывать в него:
