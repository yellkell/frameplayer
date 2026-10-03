//! Ambisonics: format conversion, head-tracked rotation and binaural rendering.
//!
//! Signal flow for an ambisonic track:
//! `FuMa/AmbiX input → AmbiX (ACN/SN3D) → ShRotation (head counter-rotation)
//! → SH-domain binaural filters (virtual-speaker decode ⊛ HRIRs, pre-summed)
//! → stereo`. Orders 1–3 are supported; higher channels are ignored.

pub mod binaural;
pub mod decoder;
pub mod hrtf;
pub mod rotation;
pub mod sh;

pub use binaural::BinauralRenderer;
pub use decoder::VirtualSpeakerDecoder;
pub use hrtf::SphericalHeadHrtf;
pub use rotation::ShRotation;
