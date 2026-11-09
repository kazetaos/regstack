# regstack

Manage registry files in layers for portability between Wine versions.

Wine keeps a prefix's whole registry in a few large files, so the defaults Wine sets up,
the settings an app ships with, and whatever the app changed while running all end up mixed
together. That makes it hard to update the base without losing the app's changes, or to keep the
app's changes without freezing an old base in place. Saving the prefix's registry also means saving
megabytes that are almost entirely defaults. `regstack` solves this by keeping each of those parts
apart and only saving the part that belongs to the app.

`regstack` keeps a Wine prefix's registry (`system.reg`, `user.reg`, `userdef.reg`) as a stack of
layers instead of one file: for example a base registry, an app's own settings, and what changed
while the app ran. A launcher merges the layers into the prefix before starting Wine, and after the
app exits writes what Wine's files changed relative to the layers into a new top layer. The top
layer holds only the app's changes, so it keeps working when the base layers are updated.

For example, [Kazeta](https://github.com/kazetaos/kazeta)'s Windows runtimes use these layers:

1. `.kazeta/share/registry/<hive>.d/*.reg` in name order: the runtime's `00-runtime.reg`, then the
   cart's own files
2. `.kazeta/var/registry/<hive>` in the save: what changed while the cart ran

```
regstack merge OUT LAYER...                    write LAYER1 + LAYER2 + ... as one registry file
regstack diff [--keep] CURRENT OUT LAYER...    write what CURRENT changes relative to the layers;
                                                 --keep leaves out deletions (for a registry
                                                 written with another Wine version's keys)
```

Layers are Wine registry files (as Wine writes them, or snippets with just key sections). On top of
Wine's format they may delete things, as in regedit's format:

```
[-Software\\Key]       deletes the key and its subkeys
"name"=-             deletes a value
```

Keys and value names are compared case-insensitively, as Wine does. Wine's key timestamps and
`#time` lines are ignored, so keys Wine only touched don't show up as changes, and diffs give keys
the timestamp 0, so the same changes always give the same file. Hex values split over several
lines are joined. Wine doesn't write keys that only hold subkeys, so a diff doesn't count those as
deleted. Files are written to `OUT.tmp` and renamed, so an interrupted write leaves the old file.

## Building

```
cargo build --release
```

Pushing a `v*` tag builds `regstack-x86_64-linux` and publishes it as a release.
