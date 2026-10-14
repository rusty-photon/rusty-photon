"""Add a PHD2 profile for the rig's QHY5III715C with no mount, cloned from
the Simulator profile, and make it current. Run only while PHD2 is stopped.

Usage: python3 addprofile.py <path to .PHDGuidingV2>
"""
import re
import sys

path = sys.argv[1]
text = open(path).read()
lines = text.split("\n")

NEW = 3
NAME = "QHY5III715C guide (no mount)"
if f"[profile/{NEW}]" in text:
    sys.exit(f"profile/{NEW} already exists")

# Collect the Simulator profile's sections (profile/2 and its children).
out = []
clone = []
in_p2 = False
section = None
for line in lines:
    m = re.match(r"^\[(.*)\]$", line)
    if m:
        section = m.group(1)
        in_p2 = section == "profile/2" or section.startswith("profile/2/")
    if in_p2:
        clone.append(line)

OVERRIDES = {
    "profile/3": {"name": NAME, "AutoLoadCalibration": "0"},
    "profile/3/camera": {
        "LastMenuchoice": "QHY Camera",
        "pixelsize": "1.45",
        "AutoLoadDarks": "0",
        "AutoLoadDefectMap": "0",
        "SaturationADU": "0",
    },
    "profile/3/scope": {"LastMenuChoice": "None", "LastAuxMenuChoice": "None"},
    "profile/3/stepguider": {"LastMenuChoice": "None"},
    "profile/3/rotator": {"LastMenuChoice": "None"},
    "profile/3/frame": {"focalLength": "360"},
}
DROP_SECTIONS = {"profile/3/overlay", "profile/3/overlay/slit"}

new_lines = []
section = None
for line in clone:
    m = re.match(r"^\[profile/2(.*)\]$", line)
    if m:
        section = f"profile/{NEW}{m.group(1)}"
        if section in DROP_SECTIONS:
            continue
        new_lines.append(f"[{section}]")
        continue
    if section in DROP_SECTIONS:
        continue
    kv = line.split("=", 1)
    if len(kv) == 2 and section in OVERRIDES and kv[0] in OVERRIDES[section]:
        new_lines.append(f"{kv[0]}={OVERRIDES[section][kv[0]]}")
    else:
        new_lines.append(line)

# Insert after the last profile/2 section line, before [graph].
idx = lines.index("[graph]")
lines[idx:idx] = new_lines
text = "\n".join(lines)
text = re.sub(r"^currentProfile=\d+$", f"currentProfile={NEW}", text, flags=re.M)
open(path, "w").write(text)
print("\n".join(new_lines))
