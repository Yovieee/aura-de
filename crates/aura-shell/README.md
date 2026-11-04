# Layer client rendering contract

The shell and launcher use premultiplied ARGB8888 SHM buffers. They render an
opaque fallback background by default and run on any compositor with layer-shell.

An Aura session may opt into compositor-owned panel fill by setting
`AURA_COMPOSITOR_GLASS=1` **only after its compositor supports this contract**.
In this mode the background pixels of the `aura-menu-bar`, `aura-dock`, and
`aura-launcher` layer surfaces are transparent. The compositor must render its
panel material behind the matching layer surface, using the already-composited
scene as its backdrop. Text, selected rows, and app tiles remain client content.
Clients never sample a fabricated wallpaper or pretend to have cross-surface
backdrop access. An unset flag, `0`, and all other values preserve the opaque
fallback.

`AURA_GLASS_QUALITY=reduced-transparency` forces opaque client backgrounds at
startup. `org.aura.Shell1.SetGlassQuality` updates the shell's fallback state only
after the compositor accepts the requested setting. A running launcher retains
its startup quality; the compositor is responsible for enforcing any subsequent
reduced-transparency setting for every material.

`--smoke-test` still requires real configure/attach/present frame callbacks; the
rendering opt-in does not replace readiness checks.
