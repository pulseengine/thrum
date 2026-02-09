# PulseEngine Automator — Sphinx configuration for safety documentation
#
# This project uses sphinx-needs for requirements traceability:
#   pip install sphinx-needs
#   pip install sphinxcontrib-plantuml  # for needflow diagrams

project = "PulseEngine Safety Documentation"
copyright = "2026, PulseEngine"
author = "PulseEngine"
release = "0.1.0"

extensions = [
    "sphinx_needs",
    "sphinxcontrib.plantuml",
]

# ─── sphinx-needs configuration ─────────────────────────────────────────

# Need types matching the V-model traceability chain.
# Each maps to an ASPICE SWE process area.
needs_types = [
    dict(directive="req", title="Requirement", prefix="REQ_",
         color="#BFD8D2", style="node"),        # SWE.1
    dict(directive="arch", title="Architecture", prefix="ARCH_",
         color="#DCFAC0", style="node"),         # SWE.2
    dict(directive="design", title="Detailed Design", prefix="DES_",
         color="#C0E0FA", style="node"),          # SWE.3
    dict(directive="impl", title="Implementation", prefix="IMPL_",
         color="#FED8B1", style="node"),          # SWE.3
    dict(directive="utest", title="Unit Test", prefix="UT_",
         color="#D5E8D4", style="node"),          # SWE.4
    dict(directive="itest", title="Integration Test", prefix="IT_",
         color="#DAE8FC", style="node"),          # SWE.5
    dict(directive="proof", title="Formal Proof", prefix="PRF_",
         color="#E1D5E7", style="node"),          # SWE.4
    dict(directive="review", title="Code Review", prefix="RVW_",
         color="#FFF2CC", style="node"),          # SWE.4
    dict(directive="verif", title="Verification Report", prefix="VER_",
         color="#F8CECC", style="node"),          # SWE.6
    dict(directive="rel", title="Release Artifact", prefix="REL_",
         color="#B0E0E6", style="node"),          # SWE.6
]

# Extra options available on all need types.
needs_extra_options = [
    "repo",
    "asil",
    "tcl",
    "branch",
    "commit",
    "prover",
    "gate_level",
    "reviewer",
    "aspice_process",
    "iec62304_class",
    "safety_standard",
]

# Extra link types for traceability relationships.
needs_extra_links = [
    {
        "option": "satisfies",
        "incoming": "is satisfied by",
        "outgoing": "satisfies",
    },
    {
        "option": "verifies",
        "incoming": "is verified by",
        "outgoing": "verifies",
    },
    {
        "option": "implements",
        "incoming": "is implemented by",
        "outgoing": "implements",
    },
    {
        "option": "tests",
        "incoming": "is tested by",
        "outgoing": "tests",
    },
    {
        "option": "proves",
        "incoming": "is proven by",
        "outgoing": "proves",
    },
]

# Warn if needs have broken links.
needs_report_dead_links = True

# ID regex pattern.
needs_id_regex = "^[A-Z][A-Z0-9_]*$"

# ─── General Sphinx config ──────────────────────────────────────────────

templates_path = ["_templates"]
exclude_patterns = ["_build", "Thumbs.db", ".DS_Store"]

html_theme = "alabaster"
html_static_path = ["_static"]
