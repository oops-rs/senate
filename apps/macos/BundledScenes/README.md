# Built-in scenes

`scripts/release.sh` ships every `*.senatescene` in this folder inside the
app (`Senate.app/Contents/Resources/Scenes/`), after checking each one with
`senate scene inspect` built from this checkout. Set `SENATE_BUNDLED_SCENES`
to use another folder.

The archives are not committed here: put them in this folder before a
release. For example, the Town scene from
[oops-rs/senate-town](https://github.com/oops-rs/senate-town): run
`npm run pack` there and copy its `town.senatescene` here.

The file name (without `.senatescene`) is the built-in scene's key: keep it
the same from one release to the next, so an installed copy is updated
rather than installed again. How the app installs, updates and offers
built-in scenes is in [docs/scenes.md](../../../docs/scenes.md#built-in-scenes).
