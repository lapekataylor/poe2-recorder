# Release notes

Commit subjects per release, written by `scripts/generate-release-notes.sh` and
compiled into the binary: the "What's new" dialog reads the section matching the
running version. Only `## <version>` headings and `- ` lines are parsed.

## 0.1.0
- Record Path of Exile 2 map runs from Client.txt
- Keep portal trips and Abyss sub-areas in one map-run video
- Mark deaths and time out of the map on the timeline
- Cut the hideout tail off saved map runs
- Pick up a map run already in progress when the app starts
- Add a recording resolution setting
- Rename the app to PoE Recorder
- New app icon
- Remove World of Warcraft support, the combat meter and the spell database
