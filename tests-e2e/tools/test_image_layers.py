import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

from build_context_contract import image_layer_view, only_artifact_files


class ImageLayers(unittest.TestCase):
    def view(self, layers):
        with tempfile.TemporaryDirectory() as temp:
            archive = Path(temp) / 'image.tar'
            with tarfile.open(archive, 'w') as outer:
                entries = {'manifest.json': json.dumps([{'Layers': [f'{i}/layer.tar' for i in range(len(layers))]}]).encode()}
                for i, entries_in_layer in enumerate(layers):
                    stream = io.BytesIO()
                    with tarfile.open(fileobj=stream, mode='w') as layer:
                        for name, kind in entries_in_layer:
                            member = tarfile.TarInfo(name)
                            member.type = tarfile.DIRTYPE if kind == 'dir' else tarfile.SYMTYPE if kind == 'link' else tarfile.REGTYPE
                            member.linkname = '/etc/passwd' if kind == 'link' else ''
                            member.mode = 0o755
                            layer.addfile(member)
                    entries[f'{i}/layer.tar'] = stream.getvalue()
                for name, payload in entries.items():
                    member = tarfile.TarInfo(name)
                    member.size = len(payload)
                    outer.addfile(member, io.BytesIO(payload))
            return image_layer_view(archive)

    def test_extra_source_cache_and_unrelated_directories_fail(self):
        binaries = [('build/agent_runner', 'file'), ('build/app-cli', 'file')]
        for extra in [('build/source.rs', 'file'), ('root/.cargo/cache', 'file'), ('root/.rustup', 'dir')]:
            with self.subTest(extra=extra):
                view = self.view([binaries, [extra]])
                self.assertFalse(only_artifact_files(view, ['build/agent_runner', 'build/app-cli']))

    def test_only_binaries_and_their_parent_directories_pass(self):
        view = self.view([[('build', 'dir'), ('build/agent_runner', 'file')], [('build/app-cli', 'file')]])
        self.assertTrue(only_artifact_files(view, ['build/agent_runner', 'build/app-cli']))

    def test_squashed_layer_keeps_the_same_artifact_contract(self):
        entries = self.view([[('build', 'dir'), ('build/agent_runner', 'file'), ('build/app-cli', 'file')]])
        self.assertTrue(only_artifact_files(entries, ['build/agent_runner', 'build/app-cli']))

    def test_binary_symlink_is_not_a_regular_artifact(self):
        view = self.view([[('build/agent_runner', 'link')], [('build/app-cli', 'file')]])
        self.assertFalse(only_artifact_files(view, ['build/agent_runner', 'build/app-cli']))

    def test_source_deleted_in_later_layer_still_fails(self):
        entries = self.view([[('build/agent_runner', 'file'), ('source.rs', 'file')],
                             [('build/app-cli', 'file'), ('.wh.source.rs', 'file')]])
        self.assertFalse(only_artifact_files(entries, ['build/agent_runner', 'build/app-cli']))

    def test_layer_traversal_is_rejected_without_extracting(self):
        with self.assertRaisesRegex(ValueError, 'non-relative'):
            self.view([[('../outside', 'file')], [('build/app-cli', 'file')]])


if __name__ == '__main__':
    unittest.main()
