# Visual pass against the canvas (M7 Task 11)

The desktop app compared with the boards in `docs/design/canvas/screens/`
(light and dark, Aldebaran accent), board by board, in the order the brief
gives: layout, sizes, colours, type, states. The app was run under Xvfb
with seeded downloads (web files downloading, paused, done and failed, a
torrent downloading and one sharing, a video downloading and a live
recording) at 420×640 and 1280×800. Screenshots are kept out of the repo.

Type: the app bundles Inter 4.1 (Regular, Medium, SemiBold, Bold; SIL OFL
1.1, `apps/taurus-desktop/ui/fonts/`) and uses it as its default font, so
the type checked here is what every system shows.

Not compared, on purpose: sample text and data; "Hyades" in the app where
the canvas says "Taurus"; the macOS traffic-light dots (window frame);
anything where a plan decided behaviour differently (M6b ruling R1).

`[x]` matches the canvas now. `[ ] stays:` is a difference kept on
purpose, with the reason.

## Shared changes (every board)

- Theme tokens are the canvas's: grounds, toolbar, panel, line, field and
  input, text, muted, chip, track, ok, err, hover; the six accents with
  base, strong (button fill), ink (accent-coloured text), soft and sel in
  light and dark; the file-type colours light and dark plus their middle
  tone. Primary buttons, check boxes, radios and switches take the strong
  accent (white text on it meets AA in both schemes).
- Icons use the canvas's paths (1.8 px stroke); new: list, globe, inbox,
  alert, up, arrow-down, clock, copy, search.
- New window chrome (`ui/chrome.slint`): a 44 px header bar (tinted, title
  centred 13/600, Back on the left, actions on the right) and a tinted
  footer bar.
- The switch's off colour is #8E8E99 light and #737380 dark (the canvas's #6E6E79 is 2.99:1 on the dark panel; #737380 reaches the 3:1 for controls).
- Widgets: 32 px buttons with 8 px corners (primary, outlined, plain,
  muted, quiet; optional icon), segmented control (28 px segments, the
  chosen one raised with a shadow), switches 32/36/40 px with a white
  knob, 16 px check boxes, cards with 11 px corners, kind badges with
  corners at a quarter of their size, 3 px progress bars in Advanced rows.

## Simple · First launch (`SimpleWelcome`)

- [x] Layout and order — settings at the top right, mark, title, line, link field, tip.
- [x] Sizes — mark 68 px on a 112 px tinted square, 46 px link field with rounded ends.
- [x] Colours — mark in the strong accent on the selection tint, field border and fill tokens.
- [ ] stays: Type — "Hyades / Downloads made simple." and one tip line are the M4 plan's copy; the canvas's "Paste a link to start" and three tips are not taken over (behaviour/copy by plan).
- [ ] stays: States — the field's button is a round download button inside the field, not a separate "Paste" pill: it sends what is typed, it does not read the clipboard (M4 behaviour).

## Simple · Downloads (`SimpleMain`)

- [x] Layout and order — header bar (name, settings), link field always shown, banners, tabs, list, footer with the time left and Pause all.
- [x] Sizes — 44 px header and field, 16 px side padding, round 36 px row actions.
- [x] Colours — kind colours, progress by type, ringed actions on the panel colour.
- [x] Type — names 14/500, status 12 muted.
- [ ] stays: States — the tabs fill the width in equal halves with count chips (long translations, Task 12), and the rows keep the M4 remove (×) action beside the main one.

## Simple · Add a link (`SimpleAdd`)

- [x] Colours — tokens and the strong accent on Download.
- [x] Type — title 600, muted byline.
- [ ] stays: Layout — Add stays a screen with a header and Cancel / Download at the bottom (M4 flow), not the canvas's large thumbnail and full-width Download pill.
- [ ] stays: Sizes — follow the screen layout above.
- [ ] stays: States — quality is cycled with "Other quality" (M4 plan), not a select.

## Simple · Settings (`SimpleSettings`)

- [x] Layout and order — cards for folder, speed, when done, links and startup, advanced.
- [x] Sizes — 40×24 px switches, cards with 11 px corners.
- [x] Colours — switch on in the strong accent, off in the canvas grey.
- [x] Type — 14 px labels, muted hints.
- [ ] stays: States — "When downloads finish" is a list with a check (M4 plan), not a select; the header keeps "← Settings" instead of "‹ Done" with a large title.

## Advanced compact · Downloads (`CompactMain`)

- [x] Layout and order — header bar (name, settings), link row (field with link icon, Add, Several links), tabs with counts, list, footer (↓ total, ↑, when done, Pause all).
- [x] Sizes — 44 px header, 36 px link row, 28 px tabs on a 9 px track, rows 9/8 px padding with 32 px badges, 3 px bars, 32 px row actions, 40 px footer.
- [x] Colours — source chips in their kind's colours (HTTP blue, Torrent teal, yt-dlp violet), bars by state (kind colour downloading, red recording or failed, grey paused, green done), selected row on the accent selection tint.
- [x] Type — names 13/500, percent 12/600 (LIVE and Failed in red), status 11.5 muted.
- [ ] stays: States — selecting a row opens its details over the list (M6b plan: "Back selects nothing"), not inline below the row; Add is off while the field is empty; a fourth button (Grab links from a page…) sits next to Several links so grab links stays reachable from the list. With "Do nothing" chosen the footer's when-done button shows only the moon (as on the canvas), not the M6b plan's "When done" label; its accessible name is still "When done", and any other choice shows its label ("Then sleep", …). Under 400 px Pause all / Resume all shows only its icon and a long when-done label elides, so the footer fits from the 360 px minimum. Resume all is offered once something is paused and nothing downloading, queued or waiting is left; failed, sharing and recording downloads do not count.

## Advanced compact · Add link (`CompactAdd`)

- [x] Layout and order — header bar, content, tinted footer with Cancel and Download (with its icon).
- [x] Sizes — 12 px padding and gaps, 32 px footer buttons.
- [x] Colours — tokens, strong accent on Download, selection tint on notes.
- [x] Type — 13 px body, 600 titles.
- [ ] stays: States — Add stays a screen of its own (M4/M6b navigation) instead of a card over the list; quality is a radio list (every height the video has, M6b plan) instead of five tiles.

## Advanced compact · Settings (`CompactSettings`)

- [x] Layout and order — header bar with Back and the title, each section on a card.
- [x] Sizes — 12 px page padding, 18 px between cards, 32 px switches.
- [x] Colours — panel cards on the ground, switch and accent tokens.
- [x] Type — section titles 14/700, labels 13/500, hints 11.5 muted.
- [ ] stays: Layout detail — sections keep the M6b order (Downloads first, Engines later) and the section title sits inside its card; values use segmented controls and spin boxes (M6b plan) instead of selects.

## Advanced compact · Torrent files (`CompactTorrent`)

- [x] Layout and order — header bar, torrent card, files, sharing, save to, footer.
- [x] Sizes — 12 px padding, 16 px check boxes, 32 px footer buttons.
- [x] Colours — torrent teal badge, check boxes in the strong accent.
- [x] Type — 13 px rows, muted sizes.
- [ ] stays: States — no "Download in order" switch (M6b plan: librqbit always downloads in order); the picker is a tri-state list grouped by folder with All / None (M6b plan) instead of a card of 44 px rows with small kind badges; the title is "New download" (one Add screen for every kind).

## Advanced compact · Links on a page (`CompactGrab`)

- [x] Layout and order — header bar, page link and Find files, Subfolders, type chips with counts, extension filter and Select all shown, list, footer with the count and Download N.
- [x] Sizes — 12 px padding, chips, 32 px footer buttons.
- [x] Colours — chips in their kind's colours, the chosen chip on the selection tint.
- [x] Type — 13 px names, muted sizes.
- [ ] stays: Layout detail — Subfolders keeps its M6b switch row (with its hint) above the chips rather than beside the extension field; rows show no folder line (the M6b row model has none); no Cancel in the footer (Back in the header).

## Advanced compact · Several links (`CompactBatch`)

- [x] Layout and order — header bar, text box, mirrors switch, parsed lines with kind chips, sign-in details, footer with Cancel and Add N downloads.
- [x] Sizes — 12 px padding, 32 px footer buttons.
- [x] Colours — kind chips, "Not a link" in red.
- [x] Type — 13 px lines.
- [ ] stays: Layout detail — lines are not numbered and have no dividers; the mirrors switch sits above the list; there is no "Start now" box (not part of the M6b screen: batches start as the queue allows).

## Advanced compact · When done & speed (`CompactWhenDone`)

- [x] Layout and order — card above the footer at the right: heading, five radios with their icons, the 60-second note, a line, Speed, segmented speeds.
- [x] Sizes — 292 px card, 14 px padding, 12 px corners, 32 px rows, 15 px radios and icons.
- [x] Colours — radio in the strong accent, muted icons; the footer button takes the selection tint and accent ink once a choice is made.
- [x] Type — 13 px rows, 12/600 muted headings.
- [ ] stays: States — the speeds are Gentle / Balanced / Fastest / Custom (M6b plan), with no description line under them.

## Wide · Downloads (`Main`)

- [x] Layout and order — sidebar (mark and name, Library and Sources with icons and counts, Settings at the bottom), top bar (title, search, New download), list, footer.
- [x] Sizes — 220 px sidebar (180 px under 1100 px), 34 px entries, 56 px top bar, a search field of 120 to 240 px that gives way first, 32 px New download, a 260 to 300 px detail pane: everything fits from 900 px up.
- [x] Colours — tinted sidebar, chosen entry on the selection tint in accent ink, sources in their kind's colours.
- [x] Type — 13/500 entries, 11/600 muted section titles, 18/700 title.
- [ ] stays: Rows and detail — the list uses the compact row (one row design for both layouts) rather than the canvas's two-column row; the sidebar has no speed graph card (no speed history is kept); the top bar has no Pause all / Remove selected icons (Pause all is in the footer; there is no multi-select); New download opens the link row (M6b "+" panel); the search field is the std text field (no magnifier, no tinted fill); the title stays "Downloads" with no item count line; the detail pane keeps its M6b buttons.

## Wide · New download (`AddDownload`)

- [x] Layout and order — the Advanced Add screen: header bar, details, options, save to, footer.
- [x] Colours — tokens and accent as in Add link.
- [x] Type — as in Add link.
- [ ] stays: Sizes — the form spans the window (no centred column or two-column options).
- [ ] stays: States — as in Add link.

## Wide · Settings (`Settings`)

- [x] Layout and order — section list on the left, one section on the right.
- [x] Sizes — 220 px section list, a content column of at most 720 px, cards with 11 px corners.
- [x] Colours — tinted section list with a line, chosen section on the selection tint in accent ink.
- [x] Type — 13/500 entries, chosen 600.
- [ ] stays: States — the header bar with Back stays (one Settings screen for both widths) instead of "‹ Back to downloads" in the section list; sections have no icons and keep the M6b names ("General" for "Browser", M6b plan).

## Pseudo-locale (M7 Task 12)

The app was run under Xvfb with `HYADES_LANG=en_XA` (every string padded
to at least 1.4× and wrapped in ⟦…⟧) at 420×640, with the same seeded
downloads, through every board above that fits that width (Simple
welcome, downloads, add and settings; compact downloads, add, torrent,
grab links, several links, when done and settings; the compact details
of a failed, a torrent, a video and a live download) and every dialog:
sign-in (with "wrong password"), remove, choose files, the update banner
ready / available / couldn't update, the tools banner fetching and failed,
the clipboard offer, the when-done countdown, and the footer with Resume
all and "Then shut down". Long pages (settings, add) were also shot in a
taller window so nothing below the fold was missed. English was shot
again afterwards to check nothing changed there.

Rules: buttons and tabs elide; sentences wrap; a row with an action keeps
the action at its natural width and its text takes the rest; no fixed
widths for English.

- [x] Speed and ratio segments (when-done card, Settings → Downloads and
  Torrents): labels ran over the next segment; now each label is bounded
  by its segment and elides.
- [x] Grab links type chips: the row ran past the right edge; chip labels
  now elide (counts stay), and "All" keeps its natural width.
- [x] Advanced Add card (video, file or torrent): with a title on two
  lines, the last line ran below the card. The card's side padding now
  sits on its row: wrapped text inside a `Card` is measured without the
  card's own side padding, so it came out a line short.
- [x] Text buttons elide when squeezed, so a long label never clips or
  pushes another button out of the window (actions beside text keep their
  width: the text gives way first).
- [x] Detail labels (Saved to, Until ratio, …) wrap instead of being cut
  at their 96 px column.
- Checked and fine as they were: compact list rows, tabs, link row and
  footer; Simple rows, tabs and footer; header titles; every dialog and
  banner above (their sentences already wrap and their buttons fit).
- `pseudo_fits_at_420` (tests/ui.rs) checks, under `en_XA` at 420 px,
  the segment labels, the grab chips and buttons, and the Add card.
