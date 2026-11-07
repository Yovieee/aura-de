"""Regression checks for Rust module paths without invoking Cargo."""
import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from verify_workspace import verify_paths


class WorkspacePathTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="aura-workspace-paths-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.src = self.root / "crates/example/src"
        self.src.mkdir(parents=True)

    def write(self, name, contents="// fixture\n"):
        path = self.src / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)

    def verify(self):
        with contextlib.redirect_stdout(io.StringIO()):
            verify_paths(self.root)

    def test_xwayland_restricted_module_keeps_its_explicit_path(self):
        self.write("xwayland.rs", '#[path = "x11_initial_state.rs"]\npub(crate) mod initial_state;\n')
        self.write("x11_initial_state.rs")
        self.verify()

    def test_visibility_and_multiline_attributes_preserve_explicit_paths(self):
        self.write("physical.rs")
        for visibility in ("", "pub ", "pub(crate) ", "pub(super) ", "pub(in crate::compat) "):
            with self.subTest(visibility=visibility):
                self.write("lib.rs", '#[cfg(\n    feature = "xwayland"\n)]\n'
                           '#[ path =\n    "physical.rs"\n]\n'
                           f"{visibility}mod logical;\n")
                self.verify()

    def test_ordinary_file_and_directory_modules_still_resolve(self):
        self.write("lib.rs", "pub(crate) mod file;\npub(super) mod directory;\n")
        self.write("file.rs")
        self.write("directory/mod.rs")
        self.verify()

    def test_missing_explicit_path_does_not_fall_back_to_logical_name(self):
        self.write("lib.rs", '#[path = "missing.rs"]\npub(crate) mod logical;\n')
        self.write("logical.rs")
        with self.assertRaisesRegex(AssertionError, "Unresolved module logical"):
            self.verify()

    def test_missing_ordinary_module_still_fails(self):
        self.write("lib.rs", "pub(crate) mod missing;\n")
        with self.assertRaisesRegex(AssertionError, "Unresolved module missing"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
