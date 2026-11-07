# agent-log

CLI nástroj v Rustu pro zaznamenávání zpráv — AI agent jím loguje, jakou akci právě provádí.

## Použití

```bash
# Zapsat akci (vytvoří agent.log v aktuální složce)
agent-log add "Čtení konfiguračního souboru"

# Vypsat všechny záznamy
agent-log list

# Vypsat jen posledních 5 záznamů
agent-log list --limit 5

# Vymazat log
agent-log clear
```

Výstup:

```
[2026-11-07 13:50:10] Čtení konfiguračního souboru
```

## Volby

- `--file <cesta>` — vlastní cesta k logu (výchozí `agent.log`)
- Proměnná prostředí `AGENT_LOG_FILE` — stejný efekt jako `--file`

## Formát logu

Záznamy se ukládají jako JSON Lines (jeden JSON objekt na řádek), takže je lze snadno zpracovat i strojově:

```json
{"ts":"2026-11-07 13:50:10","message":"Čtení konfiguračního souboru"}
```

## Sestavení

```bash
cargo build --release
# binárka: target/release/agent-log
```
