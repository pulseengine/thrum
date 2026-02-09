PulseEngine Safety Documentation
=================================

Functional safety traceability for the PulseEngine toolchain:

- **meld** — Static component fusion (Rocq 9.0 verified)
- **loom** — WebAssembly optimizer (Z3 + Rocq verified, ASIL B)
- **synth** — ARM code synthesizer (Z3 + Coq verified, ASIL D)

.. toctree::
   :maxdepth: 2
   :caption: Traceability

   traceability/loom
   traceability/meld
   traceability/synth

.. toctree::
   :maxdepth: 2
   :caption: Tool Qualification

   qualification/tool_classification
   qualification/oss_management

.. toctree::
   :maxdepth: 2
   :caption: Standards Compliance

   standards/iso26262
   standards/iec62304
   standards/aspice
