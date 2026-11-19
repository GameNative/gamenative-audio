# Vendored ALSA headers

`alsa/` is a copy of alsa-lib's public headers (LGPL-2.1-or-later), taken from the
1.2.x headers this container's `libasound.so.2` was built against. They are vendored
so that the plugin builds without a dependency on an external source tree.

Only `asoundlib.h`, `pcm.h`, `pcm_external.h`, `pcm_ioplug.h`, `pcm_extplug.h` and
`global.h` are used, but their siblings are kept so the include graph stays exactly
as upstream wrote it: `global.h` keys on the `PIC` macro, and a hand-trimmed copy is
how a plugin ends up referencing `snd_dlsym_start` and failing to `dlopen`.
