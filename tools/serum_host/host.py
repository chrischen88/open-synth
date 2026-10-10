"""Host Serum 2 through pedalboard: set params, read the decoded processor state, inject presets, render.

Notes learned the hard way:
* Parameter writes only reach ``raw_state`` after audio has been processed (``settle``).
* Assigning ``raw_state`` fully resets host parameter values to the loaded state, which is
  much cheaper (~0.1 s) than reloading the plugin (~5 s).
* The state stores only non-default values; a missing key means "default".
"""
import os
import wave

from . import state

DEFAULT_VST3 = "/Library/Audio/Plug-Ins/VST3/Serum2.vst3"
SR = 44100


def vst3_path():
    return os.environ.get("SERUM_VST3", DEFAULT_VST3)


class Serum:
    def __init__(self, path=None, plugin_name="Serum 2"):
        import pedalboard
        self.plugin = pedalboard.load_plugin(path or vst3_path(), plugin_name=plugin_name)
        self.parameters = self.plugin.parameters
        self.settle()
        self.baseline = self.plugin.raw_state

    # -- processing / state ------------------------------------------------------------
    def settle(self, duration=0.1):
        """Process silence so pending parameter changes are committed to the state."""
        self.plugin([], duration=duration, sample_rate=SR)

    def reset(self, raw=None):
        """Restore a raw_state (default: the freshly-loaded baseline) and settle."""
        self.plugin.raw_state = raw or self.baseline
        self.settle()

    def set(self, name, raw_value):
        self.parameters[name].raw_value = float(raw_value)

    def display(self, name):
        return self.parameters[name].string_value

    def processor_state(self):
        """Decoded IComponent CBOR (dict)."""
        return state.decode_raw_state(self.plugin.raw_state)[1]

    def controller_state(self):
        return state.decode_raw_state(self.plugin.raw_state)[3]

    def set_processor_state(self, proc_obj):
        """Replace the processor state with ``proc_obj`` (keeps controller state and metadata)."""
        cm, _, em, eo = state.decode_raw_state(self.plugin.raw_state)
        comp = state.pack(cm, proc_obj)
        ctrl = state.pack(em, eo)
        self.plugin.raw_state = state.build_raw_state(comp, ctrl)
        self.settle(0.3)

    # -- presets -----------------------------------------------------------------------
    def inject_preset(self, preset_path):
        """Load a .SerumPreset by overlaying its modules on the current processor state.

        Keys the preset lacks keep their current value.  The file is only read, never moved.
        """
        pmeta, preset, _ = state.sp.read(preset_path)
        cm, cur, em, eo = state.decode_raw_state(self.plugin.raw_state)
        merged = {k: (preset[k] if k in preset else cur[k]) for k in cur}
        for k in preset:
            merged.setdefault(k, preset[k])
        comp = state.pack(cm, merged)
        ctrl = state.pack({**em, "presetName": pmeta.get("presetName", "")}, eo)
        self.plugin.raw_state = state.build_raw_state(comp, ctrl)
        self.settle(0.3)

    # -- audio -------------------------------------------------------------------------
    def render_notes(self, notes=((48, 0.0, 1.5),), velocity=100, duration=2.0):
        """Render MIDI notes [(note, start_s, length_s)] -> float array (channels, samples)."""
        import mido
        msgs = []
        for note, start, length in notes:
            msgs.append(mido.Message("note_on", note=note, velocity=velocity, time=start))
            msgs.append(mido.Message("note_off", note=note, time=start + length))
        return self.plugin(msgs, duration=duration, sample_rate=SR)


def write_wav(path, audio, sample_rate=SR):
    """Write a (channels, samples) float array as 16-bit PCM."""
    import numpy as np
    pcm = (np.clip(audio.T, -1, 1) * 32767).astype("<i2")
    with wave.open(path, "wb") as w:
        w.setnchannels(pcm.shape[1])
        w.setsampwidth(2)
        w.setframerate(sample_rate)
        w.writeframes(pcm.tobytes())
