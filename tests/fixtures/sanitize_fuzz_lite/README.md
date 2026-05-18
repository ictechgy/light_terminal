# sanitize fuzz-lite seed corpus

Small deterministic byte seeds for `sanitize::terminal_capture`. These are not a
coverage-guided fuzzer; they are regression fixtures for escape parser states that
have historically mattered to `lterm` report surfaces.
