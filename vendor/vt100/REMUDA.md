This directory contains vt100 0.16.2 under its upstream MIT license.
Remuda adds OSC 8 metadata to terminal cells. The parser stores a shared
target on each printed glyph and clears it with the cell on overwrite or
erase. It does not open the target or include it in ANSI screen output.
The vendored crate omits upstream test-only dependencies.
