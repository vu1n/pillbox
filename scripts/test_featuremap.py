"""Exercise the vendored helper's public CLI against disposable Git repositories."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HELPER = Path(__file__).with_name('featuremap.py').resolve()


class FeatureMapCliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git('init', '-q')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('config', 'user.name', 'Feature map fixture')
        self.write('src/run.py', 'def run_turn(): pass\n')
        self.write('tests/test_run.py', 'def test_exact_retry(): pass\n')
        self.write('docs/features/README.md', '# Index\n')
        self.map()
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture')

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.root), *args], text=True)

    def write(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def map(self, name='run', body=None):
        self.write(f'docs/features/{name}.md', body or '''---
id: run
paths:
  - "src/**"
entry:
  - "src/run.py:run_turn"
tests:
  - "tests/test_run.py:test_exact_retry"
---
# Run a turn
## Gotchas
Exact retries must reuse the original result.
''')

    def cli(self, *args):
        return subprocess.run(
            [sys.executable, str(HELPER), '--root', str(self.root), *args],
            capture_output=True, text=True,
        )

    def test_valid_map_and_changed_file_lookup(self):
        result = self.cli('check')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn('1 features, 0 broken pointer(s)', result.stdout)
        result = self.cli('features', 'src/run.py', 'unmapped.txt')
        self.assertEqual(result.returncode, 0, result.stderr)
        for expected in ['## run:', 'run_turn', 'test_exact_retry', 'Exact retries', 'Unmapped (1): unmapped.txt']:
            self.assertIn(expected, result.stdout)

    def test_dead_file_symbol_and_glob_are_reported_together(self):
        self.write('src/run.py', 'renamed = True\n')
        (self.root / 'tests/test_run.py').unlink()
        self.git('rm', '--cached', 'src/run.py')
        result = self.cli('check')
        self.assertEqual(result.returncode, 1)
        for expected in ['matches no tracked file', 'not found in the file', 'no file tests/test_run.py']:
            self.assertIn(expected, result.stdout)

    def test_duplicate_id_and_missing_paths_fail(self):
        self.map(name='duplicate')
        self.map(name='no-paths', body='---\nid: orphan\n---\n# Orphan\n')
        result = self.cli('check')
        self.assertEqual(result.returncode, 1)
        self.assertIn("id 'run' is also used", result.stdout)
        self.assertIn('no paths', result.stdout)

    def test_no_tests_is_advisory_and_invalid_frontmatter_is_error(self):
        self.map(body='---\nid: run\npaths:\n  - "src/**"\n---\n# Run\n')
        result = self.cli('check')
        self.assertEqual(result.returncode, 0)
        self.assertIn('lists no tests', result.stdout)
        self.map(body='# Missing frontmatter\n')
        result = self.cli('check')
        self.assertEqual(result.returncode, 2)
        self.assertIn('no frontmatter', result.stderr)

    def test_base_lookup_includes_the_old_side_of_a_move(self):
        base = self.git('rev-parse', 'HEAD').strip()
        self.git('mv', 'src/run.py', 'moved.py')
        self.git('commit', '-qm', 'move implementation out of mapped path')
        result = self.cli('features', '--base', base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('## run:', result.stdout)
        self.assertIn('Unmapped (1): moved.py', result.stdout)


if __name__ == '__main__':
    unittest.main()
