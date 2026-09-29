#!/usr/bin/env python3
"""Write fake Path of Exile 2 Client.txt lines so the recorder can be tested
without the game.

The line formats below are a best guess based on how PoE logs look. Once you
can play, compare them with your real Client.txt and fix the templates in the
"LINE FORMATS" section; nothing else in this file needs to change.

Run it from the repository root. Examples:
    python3 scripts/fake-poe2-log.py                      # the full demo run
    python3 scripts/fake-poe2-log.py --scenario portal    # one map with a mid-map portal
    python3 scripts/fake-poe2-log.py --speed 60           # 60x faster than real time
    python3 scripts/fake-poe2-log.py --instant            # write everything at once
    python3 scripts/fake-poe2-log.py --list               # show the scenarios

Watch the output in a second terminal with:
    tail -f test-logs/Client.txt
"""

import argparse
import random
import sys
import time
from datetime import datetime, timedelta
from pathlib import Path

# ---------------------------------------------------------------- settings --

CHARACTER = "TestExile"
HIDEOUT = "HideoutFelled"
TOWN = "G1_town"
DEFAULT_OUTPUT = Path("test-logs") / "Client.txt"

# --------------------------------------------------------- LINE FORMATS --
# Each real line starts with: date time, milliseconds since the game started,
# an 8-character hex id, then [LEVEL Client <process id>].

GENERATING = 'Generating level {level} area "{area}" with seed {seed}'
SCENE = "[SCENE] Set Source [{name}]"
SLAIN = ": {character} has been slain."
NOISE = [
    ("DEBUG", "[SHADER] Delay: ON"),
    ("INFO", "Connecting to instance server at 10.0.0.1:6112"),
    ("INFO", "Connect time to instance server was 21ms"),
    ("INFO", "[WINDOW] Cursor released"),
    ("INFO", "#Global: SomePlayer: anyone selling waystones?"),
]

# ---------------------------------------------------------------- scenarios --
# Each step is (seconds of game time to wait first, action, arguments).
# Seeds identify a map instance: going back through a portal reuses the seed,
# which is how the recorder can tell "same map" from "new map".

SCENARIOS = {
    "simple": (
        "Enter a map, clear it, go back to the hideout.",
        [
            (0, "enter", ("hideout",)),
            (5, "enter", ("map", "MapBluff", 70, 1001)),
            (120, "noise", ()),
            (120, "enter", ("hideout",)),
        ],
    ),
    "death": (
        "Enter a map, die once, respawn in the map, finish it.",
        [
            (0, "enter", ("hideout",)),
            (5, "enter", ("map", "MapHiddenGrotto", 72, 2002)),
            (60, "death", ()),
            # After dying you choose to respawn at the checkpoint, which loads
            # the same instance again.
            (5, "enter", ("map", "MapHiddenGrotto", 72, 2002)),
            (90, "enter", ("hideout",)),
        ],
    ),
    "portal": (
        "Portal to the hideout mid-map for 20s, return, finish the map.",
        [
            (0, "enter", ("hideout",)),
            (5, "enter", ("map", "MapWillow", 74, 3003)),
            (60, "enter", ("hideout",)),  # sell loot, should NOT split the video
            (20, "noise", ()),
            (0, "enter", ("map", "MapWillow", 74, 3003)),  # same seed = same map
            (90, "enter", ("hideout",)),
        ],
    ),
    "abandon": (
        "Enter a map, portal out and never come back.",
        [
            (0, "enter", ("hideout",)),
            (5, "enter", ("map", "MapCrypt", 70, 4004)),
            (45, "enter", ("hideout",)),
            (180, "noise", ()),  # sits in hideout past any grace period
        ],
    ),
    "back-to-back": (
        "Two maps in a row with a quick hideout stop between them.",
        [
            (0, "enter", ("hideout",)),
            (5, "enter", ("map", "MapBluff", 75, 5005)),
            (80, "enter", ("hideout",)),
            (10, "enter", ("map", "MapSwamp", 75, 6006)),  # new seed = new map
            (80, "enter", ("hideout",)),
        ],
    ),
    "demo": (
        "Everything: a town visit, a map with a death and a mid-map portal, "
        "then a second map.",
        [
            (0, "enter", ("town",)),
            (10, "enter", ("hideout",)),
            (5, "enter", ("map", "MapWillow", 74, 7007)),
            (30, "noise", ()),
            (30, "death", ()),
            (5, "enter", ("map", "MapWillow", 74, 7007)),
            (40, "enter", ("hideout",)),  # portal out mid-map
            (20, "enter", ("map", "MapWillow", 74, 7007)),  # and back in
            (60, "enter", ("hideout",)),  # map done
            (15, "enter", ("map", "MapBluff", 75, 8008)),
            (90, "enter", ("hideout",)),
        ],
    ),
}

# ------------------------------------------------------------------ writer --


class FakeClientLog:
    def __init__(self, path, speed, instant):
        self.path = path
        self.speed = speed
        self.instant = instant
        self.pid = random.randint(1000, 60000)
        self.started = time.monotonic() - random.randint(60, 3600)
        # With --instant nothing waits, so the skipped time is added to the
        # timestamps instead. That keeps map durations realistic in the file.
        self.skipped = 0.0

    def write(self, level, message):
        now = datetime.now() + timedelta(seconds=self.skipped)
        uptime_ms = int((time.monotonic() - self.started + self.skipped) * 1000)
        line = (
            f"{now:%Y/%m/%d %H:%M:%S} {uptime_ms} {random.getrandbits(32):08x} "
            f"[{level} Client {self.pid}] {message}"
        )
        # Append and close each time, like the game flushing its log, so a
        # watcher reading the file sees every line as soon as it is written.
        with self.path.open("a", encoding="utf-8", newline="\r\n") as log:
            log.write(line + "\n")
        print(line)

    def wait(self, game_seconds):
        if self.instant:
            self.skipped += game_seconds
        elif game_seconds:
            time.sleep(game_seconds / self.speed)

    def enter(self, kind, area=None, level=None, seed=None):
        if kind == "hideout":
            area, level, seed, scene = HIDEOUT, 1, 1, "Felled Hideout"
        elif kind == "town":
            area, level, seed, scene = TOWN, 15, 1, "Clearfell Encampment"
        else:
            scene = area
        self.write("DEBUG", GENERATING.format(level=level, area=area, seed=seed))
        self.write("INFO", SCENE.format(name=scene))

    def death(self):
        self.write("INFO", SLAIN.format(character=CHARACTER))

    def noise(self):
        self.write(*random.choice(NOISE))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--scenario", default="demo", choices=SCENARIOS)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--speed",
        type=float,
        default=10,
        help="how many times faster than real play (default 10)",
    )
    parser.add_argument("--instant", action="store_true", help="no waiting")
    parser.add_argument(
        "--fresh", action="store_true", help="empty the file before writing"
    )
    parser.add_argument("--list", action="store_true", help="list scenarios")
    args = parser.parse_args()

    if args.list:
        for name, (description, _) in SCENARIOS.items():
            print(f"{name:14} {description}")
        return
    if args.speed <= 0:
        parser.error("--speed must be above 0")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    if args.fresh:
        args.output.write_text("")
    log = FakeClientLog(args.output, args.speed, args.instant)

    description, steps = SCENARIOS[args.scenario]
    print(f"Scenario '{args.scenario}': {description}", file=sys.stderr)
    print(f"Writing to {args.output}\n", file=sys.stderr)
    try:
        for delay, action, arguments in steps:
            log.wait(delay)
            getattr(log, action)(*arguments)
    except KeyboardInterrupt:
        print("\nStopped.", file=sys.stderr)


if __name__ == "__main__":
    main()
