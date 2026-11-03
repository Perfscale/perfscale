# Агенты кодирования

Две команды подключают perfscale к coding-агентам (Claude Code, Codex,
Copilot, opencode, VS Code). Они зеркалят `init-skills` / `init-agents` из
Playwright.

## `perfscale init-skills`

Устанавливает skill-папки, которые обучают агента форматам perfscale. Скиллы
встроены в бинарник и копируются как есть — повторный запуск перезаписывает
их, поэтому скиллы всегда соответствуют установленной версии perfscale.

```sh
perfscale init-skills                 # ./.claude/skills/<skill>   (цикл по умолчанию)
perfscale init-skills --loop agents   # ./.agents/skills/<skill>
perfscale init-skills -g              # ~/.claude/skills/<skill> (все проекты)
```

Поставляемые скиллы:

- **perfscale-cli** — пара test/config YAML, `lint`/`run`/`schema`, плюс
  справочники: YAML, каталог действий `std/*`, профили нагрузки, пороги,
  метрики, репортёры, протоколы.
- **perfscale-library-sdk** — WASM-шаги, `defineMetric`, расширения конфига.
- **perfscale-migration-k6** — гид по миграции k6 → perfscale и таблица
  соответствия метрик.

## `perfscale init-agents`

Генерирует определения агентов для одного цикла, регистрирует MCP-сервер
perfscale и инициализирует каркас репозитория:

```sh
perfscale init-agents                            # claude (цикл по умолчанию)
perfscale init-agents --loop vscode              # один из: claude, codex,
                                                 # copilot, opencode, vscode,
                                                 # vscode-legacy
perfscale init-agents -c staging.config.yaml     # сид на основе существующего конфига
perfscale init-agents --prompts                  # также установить шаблоны промптов
```

Перезапускайте после каждого обновления perfscale — проверки версий нет.

### Что генерируется

| Цикл | Файлы агентов | MCP-конфиг |
|---|---|---|
| `claude` | `.claude/agents/perfscale-test-*.md` | `.mcp.json` |
| `codex` | `.codex/agents/perfscale-test-*.toml` | `.codex/config.toml` |
| `copilot` | `.github/agents/perfscale-test-*.agent.md` | `mcp-servers` во frontmatter + `.github/workflows/copilot-setup-steps.yml` |
| `opencode` | `.opencode/prompts/perfscale-test-*.md` | `opencode.json` |
| `vscode` | `.github/agents/perfscale-test-*.agent.md` | `.vscode/mcp.json` |
| `vscode-legacy` | `.github/chatmodes/perfscale-test-*.chatmode.md` | `.vscode/mcp.json` |

Каждый MCP-конфиг регистрирует [сервер `@perfscale/mcp`](mcp.md) как
`npx -y @perfscale/mcp` (`cmd /c npx …` на Windows). Существующие конфиги
дополняются, а не заменяются. Переход на `--loop copilot` удаляет устаревшие
`perfscale-test-*.chatmode.md` и agent-файлы старого формата
`perfscale-test-*.md`; unrelated-файлы сохраняются.

### Три агента

- **perfscale-test-planner** — превращает запрос (плюс seed-тест и опционально
  OpenAPI/HAR/PRD) в `specs/<name>.md`: поток шагов, профиль нагрузки,
  тестовые данные, SLO-пороги.
- **perfscale-test-generator** — превращает план в `tests/<name>.yaml` через
  MCP-инструменты записи (каждая запись линтится), проверяя коротким прогоном.
- **perfscale-test-healer** — воспроизводит падение на малом прогоне, чинит
  тест, перезапускает; помечает тест `skip:` с причиной, если сломан сам
  целевой сервис.

### Каркас репозитория (всегда)

- `specs/README.md` — каталог планов тестов.
- `tests/seed.yaml` — минимальный пробный тест на 1 VU, образец для новых
  тестов. «Найти или создать»: существующий seed никогда не перезаписывается.
- `config.yaml` — создаётся, только если конфига нет и `-c` не передан;
  `-c <config>` выбирает существующий конфиг (отсутствующий файл = ошибка).

### Промпты (`--prompts`)

`perfscale-{plan,generate,heal,threshold}.prompt.md` попадают в каталог
промптов цикла (`.claude/commands`, `.codex/prompts`, `.opencode/prompts`,
`.github/prompts`). Промпт `coverage` из Playwright заменён на **threshold**:
«выдержит ли цель заданную нагрузку?» — ступенчатый `run_test` до целевой
нагрузки с вердиктом HOLD / NOT HOLD по каждому SLO-порогу.

### Правила безопасности нагрузочного тестирования (вшиты в агентов)

- Прогоны генератора и хилера по умолчанию — **1 VU и короткая длительность**.
  Полная нагрузка — только через промпт `threshold` (он явно указывает
  целевую нагрузку и сначала просит подтверждение) или по явному запросу
  пользователя.
- Хилер **никогда не редактирует `thresholds:`**: ослабить SLO — не исправить.
