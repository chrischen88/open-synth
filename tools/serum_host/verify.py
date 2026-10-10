"""Load a .SerumPreset into a hosted Serum 2 and report what Serum shows, plus a render.

    python -m serum_host.verify "<preset.SerumPreset>" [host_param ...] [--wav out.wav]

Prints the host display string for each named host param (e.g. filter_1_freq_hz,
env_1_attack, a_level) as Serum itself formats it, then renders a C3 note and prints RMS/peak.
"""
import argparse

from .host import Serum, write_wav

DEFAULT_PARAMS = [
    "a_enable", "a_level", "a_unison", "a_uni_detune", "a_warp_mode", "b_enable", "b_level", "b_octave_oct", "b_wt_pos",
    "filter_1_on", "filter_1_type", "filter_1_freq_hz", "filter_1_res", "env_1_attack", "env_1_release",
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("preset")
    ap.add_argument("params", nargs="*", default=DEFAULT_PARAMS)
    ap.add_argument("--wav")
    args = ap.parse_args()

    s = Serum()
    s.inject_preset(args.preset)
    for name in args.params:
        try:
            print(f"{name:20s} {s.display(name).strip()}")
        except KeyError:
            print(f"{name:20s} <no such host param>")
    audio = s.render_notes()
    import numpy as np
    print(f"render rms {float(np.sqrt(np.mean(audio ** 2))):.4f} peak {float(np.max(np.abs(audio))):.4f}")
    if args.wav:
        write_wav(args.wav, audio)
        print("wrote", args.wav)


if __name__ == "__main__":
    main()
