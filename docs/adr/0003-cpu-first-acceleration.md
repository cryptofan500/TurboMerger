# 0003 — CPU-first, calibrated acceleration

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q3)
- **Context:** Measured on the maintainer's Ryzen 5 5500U: no ONNX Runtime GPU provider
  applies to the Radeon iGPU; 12 SMT threads are slower than 6 for ONNX; 2–3 processes
  × 2–3 threads beat 1 × 6 by 27 %.
- **Decision:** CPU by default. GPUs only through ONNX execution providers or optional
  plugins, and only when `turbomerger doctor --calibrate` measures a gain *under
  concurrent CPU load*. Every probe runs in a sacrificial subprocess.
- **Consequences:** No GPU code in the core; per-device `hwprofile.json`.
