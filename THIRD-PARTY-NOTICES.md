# Third-party notices

devserial is licensed under GPL-3.0-or-later. This file covers the third-party
works that are **embedded in the compiled binary** and therefore travel with
every copy of it. Their licences require their notices to be distributed
alongside them, which is what this file and the `licenses/` directory do.

Ordinary Rust dependencies are not listed here. They are linked rather than
embedded as data, and their licences are declared in `Cargo.lock`.

## Font software

The GUI (`--features monitor`) embeds five typefaces. One is vendored in this
repository, the other four come from the `epaint_default_fonts` crate and are
compiled in through `egui::FontDefinitions::default()`.

| Typeface | Origin | Licence | Text |
| --- | --- | --- | --- |
| JetBrains Mono 2.305 | `resources/fonts/JetBrainsMono-Regular.ttf` | OFL-1.1 | [licenses/OFL-1.1.txt](licenses/OFL-1.1.txt) |
| Noto Emoji 1.05 | `epaint_default_fonts` | OFL-1.1 | [licenses/OFL-1.1.txt](licenses/OFL-1.1.txt) |
| Ubuntu Light 0.83 | `epaint_default_fonts` | UFL-1.0 | [licenses/UFL-1.0.txt](licenses/UFL-1.0.txt) |
| Hack 3.003 | `epaint_default_fonts` | MIT, over public-domain DejaVu and Bitstream Vera | [licenses/Hack.txt](licenses/Hack.txt) |
| emoji-icon-font 1.1 | `epaint_default_fonts` | MIT | [licenses/emoji-icon-font-MIT.txt](licenses/emoji-icon-font-MIT.txt) |

Copyright, as stated by each work itself:

- JetBrains Mono: Copyright 2020 The JetBrains Mono Project Authors
  (<https://github.com/JetBrains/JetBrainsMono>)
- Noto Emoji: Copyright 2013 Google Inc. All Rights Reserved.
- Ubuntu Light: Copyright 2011 Canonical Ltd.
- Hack: Copyright (c) 2018 Source Foundry Authors / Copyright (c) 2003 by
  Bitstream, Inc. All Rights Reserved. The reserved font names are "Bitstream"
  and "Vera"; the DejaVu work the face builds on was committed to the public
  domain.
- emoji-icon-font: Copyright (c) 2014 John Slegers

OFL-1.1 and UFL-1.0 both carry a reserved-font-name clause. A modified copy of
one of these faces may not be distributed under its original name.

The four texts under `licenses/` were taken verbatim from
`epaint_default_fonts` 0.36.1 and are byte for byte identical to the copies
that crate ships. The four typeface files themselves are unchanged between
0.34.3 and 0.36.1, so the versions named above still hold.

## Platform faces loaded at run time

On macOS the GUI prefers the system UI face and reads it from
`/System/Library/Fonts` at run time. Nothing is embedded or redistributed in
that case, so no notice applies.
