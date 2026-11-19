/*
 * A link-time stand-in for libasound.
 *
 * The plugin must end up with `libasound.so.2` in its DT_NEEDED, not with a pile of
 * unresolved `snd_*` references: Wine dlopens its unixlibs with RTLD_NOW and no
 * RTLD_GLOBAL, so the plugin's own dependencies are the only scope its symbols have.
 * A real libasound at build time would do, but it would also mean checking a copy of
 * the device's library into the tree.
 *
 * So this file exists only to be linked. It carries the SONAME (`libasound.so.2`,
 * passed to the linker by build.sh) and the ten symbols the shim references. The
 * bodies are never called: at runtime the device's libasound is what gets loaded,
 * and the dynamic linker resolves these references against it. Anything this file is
 * missing shows up as a link failure rather than as a plugin that fails to load.
 *
 * Signatures are deliberately loose — this translation unit includes no ALSA
 * headers, so the compiler cannot and need not check them against the real ones. The
 * types that matter for the calling convention (pointers, `unsigned long` for
 * `snd_pcm_uframes_t`) are the same on AArch64 either way.
 */

typedef unsigned long int snd_pcm_uframes_t;

void *snd_config_get_id(void) { return 0; }

void *snd_config_iterator_first(void) { return 0; }

void *snd_config_iterator_next(void) { return 0; }

void *snd_config_iterator_end(void) { return 0; }

void *snd_config_iterator_entry(void) { return 0; }

int snd_pcm_format_physical_width(int format) { return format < 0 ? format : 0; }

int snd_pcm_ioplug_create(void) { return 0; }

int snd_pcm_ioplug_delete(void) { return 0; }

int snd_pcm_ioplug_set_param_list(void) { return 0; }

int snd_pcm_ioplug_set_param_minmax(void) { return 0; }
