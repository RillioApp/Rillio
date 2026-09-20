"""Process environment the gate scripts need before torch compiles anything.

triton-windows looks for its bundled TinyCC under the *running interpreter's*
platlib, so a venv that imports triton from another site-packages misses it
and torch.compile dies with "Failed to find C compiler". CC is derived from
where triton actually lives; an explicit CC is respected.
"""
import os
from pathlib import Path


def point_cc_at_triton_tcc() -> None:
    if os.environ.get("CC"):
        return
    import triton

    tcc = Path(triton.__file__).parent / "runtime" / "tcc" / "tcc.exe"
    if not tcc.exists():
        raise RuntimeError(f"triton's bundled tcc is missing at {tcc}; set CC explicitly")
    os.environ["CC"] = str(tcc)
