# Changelog

## v1.0.0

The first stable release of LoudTrace, a desktop app for checking track and album loudness and hearing how a streaming service would normalize it.

### Measurement

- Integrated loudness (LUFS) and true peak (dBTP) for every track, following ITU-R BS.1770 with EBU R128 gating.
- Album loudness pooled from every track's gating blocks, with the album's maximum true peak.
- Reads WAV, AIFF, FLAC, MP3, AAC/M4A (including ALAC), Ogg Vorbis and CAF files in mono or stereo. Damaged, truncated or changing files are reported instead of measured.

### Normalization preview

- Predicts Spotify Normal playback gain: a −14 LUFS target, with positive gain limited by a −1 dBTP ceiling. This is a reference model, not Spotify's official implementation.
- Individual mode gives each track its own gain; Album mode applies one shared gain that keeps the level differences between tracks.
- Normalized playback with seeking, track skipping and a separate monitor volume.

### Projects

- Projects are saved locally. Add tracks by drag and drop, reorder them, or relink a moved file.
- Analysis is incremental: only new, changed or failed tracks are measured again, and album gain is recalculated from the cached results.
- Source files are watched, and a result is cleared when its file changes.
