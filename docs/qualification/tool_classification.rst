Tool Classification
===================

ISO 26262 Part 8, Clause 11 — Tool Confidence Level
----------------------------------------------------

Each PulseEngine tool is classified based on:

- **TI (Tool Impact)**: Can the tool introduce errors?
- **TD (Tool error Detection)**: Can errors be detected downstream?

.. list-table:: TCL Determination Matrix
   :header-rows: 1
   :stub-columns: 1

   * -
     - TD1 (High detection)
     - TD2 (Medium)
     - TD3 (Low detection)
   * - TI1 (No impact)
     - TCL1
     - TCL1
     - TCL1
   * - TI2 (Can introduce errors)
     - TCL1
     - TCL2
     - TCL3

PulseEngine Tool Classifications
---------------------------------

.. list-table::
   :header-rows: 1

   * - Tool
     - TI
     - TD
     - TCL
     - ASIL Target
     - Rationale
   * - **loom**
     - TI2
     - TD2
     - TCL2
     - ASIL B
     - Optimizer transforms code (TI2). Z3 verifies each transformation,
       plus downstream synth verification provides medium detection (TD2).
   * - **synth**
     - TI2
     - TD3
     - TCL3
     - ASIL D
     - Code generator produces final binary (TI2). As the last tool in
       the chain, limited downstream detection (TD3). Maximum qualification.
   * - **meld**
     - TI2
     - TD2
     - TCL2
     - QM
     - Fuser transforms code (TI2). loom's optimization and verification
       downstream provides medium detection (TD2).

Qualification Methods
---------------------

**TCL2 (loom, meld)** — select from:

1a. Increased confidence from use
1b. Evaluation of the tool development process
1c. Validation of the software tool
1d. Development in accordance with a safety standard

**TCL3 (synth)** — must apply:

1b. Evaluation of the tool development process
1c. Validation of the software tool
1d. Development in accordance with a safety standard

Note: "Increased confidence from use" (1a) is NOT sufficient alone for TCL3.
