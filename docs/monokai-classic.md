# Monokai Classic — greenish-gray backgrounds

The olive-khaki "greenish gray" background shades of the classic Monokai
scheme (the version shipped as VS Code's built-in *Monokai* theme), ordered
darkest → lightest. Only classic shades are listed; the older Sublime-era
Monokai selection colors (`#383830`, `#49483e`) and non-classic variants are
deliberately omitted.

| Hex | RGB | Role |
| --- | --- | --- |
| `#1e1f1c` | 30,31,28 | tab well, borders, sidebar background |
| `#272822` | 39,40,34 | **editor / terminal background** |
| `#34352f` | 52,53,47 | inactive tab |
| `#3e3d32` | 62,61,50 | line highlight, list hover |
| `#414339` | 65,67,57 | selection, panels, input background |
| `#464741` | 70,71,65 | whitespace, indent guide |
| `#75715e` | 117,113,94 | comment, focus khaki |

## Chunk-header use in `bough`

The terminal background is `#272822`, so the chunk headers pick shades on
either side of it so they read against it:

| State | Color | Role |
| --- | --- | --- |
| inactive / folded | `#34352f` | inactive tab |
| active | `#464741` | selection (brighter) |