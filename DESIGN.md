# Design

Fonsviva is one family with Local Studio: same palette, same type, same voice. The tokens live in `app/src/styles.css`. Change them there and nowhere else.

## Who it is for

A person who has never run a model. Every screen answers three questions in plain words: what is happening, what it costs (time, memory, money), and what to do next. No jargon a person has to look up. Where a technical number matters (context length, tokens per second, GPU layers), show it with a plain label beside it.

## Palette

Local Studio's built-in palette, dark and light, following the OS unless the person picks one.

| Token | Dark | Light | Use |
| --- | --- | --- | --- |
| `--bg` | #110b07 | #fffdfa | Window |
| `--side` | #17100b | #fcf6ec | Sidebar |
| `--card` | #1e1711 | #fcf6eb | Cards |
| `--raised` | #251d17 | #fefbf6 | Tiles inside cards |
| `--text` | #f5eee0 | #1f1611 | Body text |
| `--gold` | #d4af5d | #896706 | The one primary action per screen, money |
| `--live` | #79c1b2 | #2f7a6b | Running, progress, live state |

Body text meets 4.5:1 on every surface it sits on. Gold marks the single primary action and spend; it is never decoration.

When the Polycarp theme crate lands (polycarp#422), the app reads the `studio` surface from `current/theme.json` and falls back to these values.

## Type

- Headings: a system serif (`--serif`: Iowan Old Style, Charter, Source Serif 4, Georgia).
- Everything else: the system UI font (`--sans`), 13px base.
- Numbers that change (speed, bytes, cost, URLs): `--mono`, so columns line up and values don't jitter.

## Layout

- Sidebar with five places: Home, Models, Connect, Cloud, Settings. First run hides it and shows one recommended model and one button.
- The first viewport always shows the thing itself: the loaded model and its measured speed, or the model to get.
- Cost is shown before anything bills and the whole time it bills, with the cap and the auto-stop rule in words.

## Platform

- macOS: overlay title bar with room for the traffic lights, drag region across the top, app menu, Cmd shortcuts.
- Windows: Fluent spacing, Ctrl shortcuts, NSIS per-user install.
- Linux: no macOS-only patterns; tray menu works on left click.
- Closing the window keeps Fonsviva in the tray, because wired apps keep using the models.

## Motion and access

- One short fade between views; nothing moves under `prefers-reduced-motion`.
- Every control is reachable by Tab and has a visible focus ring. Switches use `role="switch"`; live numbers sit in an `aria-live` region.

## Proof

A change to anything visible carries before and after screenshots at the window's minimum (860x560) and default (1100x720) sizes, in dark and light, from the built app, not only a browser.
