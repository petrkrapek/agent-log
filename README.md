# agent-log

CLI nástroj v Rustu pro zaznamenávání zpráv — AI agent jím loguje, jakou akci právě provádí.

Do zprávy nepatří hesla, tokeny ani obsah souborů. Log popisuje akci, ne data, se kterými agent pracuje.

## Použití

```bash
# Zapsat akci (vytvoří agent.log v aktuální složce, práva 0600)
agent-log add "Čtení konfiguračního souboru"

# Vypsat všechny záznamy
agent-log list

# Vypsat jen posledních 5 záznamů
agent-log list --limit 5

# Vymazat log
agent-log clear
```

Výstup používá čas RFC 3339 včetně pásma:

```
[2026-11-07T13:50:10+01:00] Čtení konfiguračního souboru
```

## Volby

- `--file <cesta>` — vlastní cesta k logu (výchozí `agent.log`)
- `--allow-outside` — povolí cestu mimo aktuální adresář
- Proměnná prostředí `AGENT_LOG_FILE` — stejný efekt jako `--file`

Cesta se jinak nesmí dostat ven z aktuálního adresáře. Symlink v cestě ani jako samotný soubor logu není povolený, ať už je `--allow-outside` zapnuté, nebo ne. Nový soubor vzniká s právy `0600` a existující log si nástroj při zápisu na tato práva srovná.

Zpráva může mít nejvýš 4096 bajtů. `list` řídicí znaky ve výpisu nahradí, aby záznam nemohl přepsat terminál. Poškozený řádek přeskočí, vypíše ho na chybový výstup a skončí nenulovým kódem. Chybějící log není prázdný log: `list` v tom případě taky skončí chybou.

## Formát logu

Záznamy se ukládají jako JSON Lines (jeden JSON objekt na řádek):

```json
{"ts":"2026-11-07T13:50:10+01:00","message":"Čtení konfiguračního souboru"}
```

## Sestavení

Vyžaduje Rust 1.85 nebo novější a Unix.

```bash
cargo build --release
# binárka: target/release/agent-log
```
