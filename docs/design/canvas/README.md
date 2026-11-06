# Design canvas ("Taurus Redesign")

The boards of the design canvas the desktop app follows, as their source
(`*.dc.html`, `canvas.json`) and as screenshots (`screens/`, light and dark,
Aldebaran accent, at each board's size from `canvas.json`). The canvas was
drawn before the product was named: read "Taurus" in it as "Hyades".

| Board | Size | Screens |
|---|---|---|
| Simple · First launch / Downloads / Add a link / Settings | 420×640 | `SimpleWelcome`, `SimpleMain`, `SimpleAdd`, `SimpleSettings` |
| Advanced compact · Downloads / Add link / Settings / Torrent files / Links on a page / Several links / When done & speed | 420×640 | `CompactMain`, `CompactAdd`, `CompactSettings`, `CompactTorrent`, `CompactGrab`, `CompactBatch`, `CompactWhenDone` |
| Wide layout · Downloads / New download / Settings | 1280×800 | `Main`, `AddDownload`, `Settings` |

`support.js` is a small stand-in for the canvas runtime, enough to open a
board in a browser (`?dark=1&accent=Jade` set its props) and to re-render
the screenshots:

```sh
node docs/design/canvas/shoot.js   # needs Playwright with Chromium
```

The boards show sample data and states; they are a visual reference
(spacing, sizes, colours, type, iconography), not a behaviour spec. Where a
plan's text and a board disagree on behaviour, the plan wins.
