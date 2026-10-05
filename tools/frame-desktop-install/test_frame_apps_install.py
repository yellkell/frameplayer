"""Tests for frame-apps-install.py: python3 -m unittest (in this folder)."""

import importlib.util
import os
import stat
import struct
import tempfile
import unittest
import zipfile
import zlib

spec = importlib.util.spec_from_file_location(
    'fai', os.path.join(os.path.dirname(os.path.abspath(__file__)), 'frame-apps-install.py'))
fai = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fai)


def other(name, appid=-5):
    return [['appid', appid], ['AppName', name], ['Exe', f'"/usr/bin/{name}"'],
            ['tags', [['0', 'Games']]]]


class Vdf(unittest.TestCase):
    def test_round_trip_keeps_every_byte(self):
        doc = [['shortcuts', [['0', other('a') + [['f', ('raw', 3, b'\x00\x00\x80?')],
                                                   ['u', ('raw', 7, b'\x01' * 8)]]],
                              ['1', other('b', 7)]]]]
        data = fai.vdf_write(doc)
        self.assertEqual(fai.vdf_parse(data), doc)
        self.assertEqual(fai.vdf_write(fai.vdf_parse(data)), data)
        self.assertTrue(data.startswith(b'\x00shortcuts\x00\x000\x00\x02appid\x00'))
        self.assertTrue(data.endswith(b'\x08\x08\x08'))

    def test_refuses_unknown_types_and_truncation(self):
        with self.assertRaises(fai.Fail):
            fai.vdf_parse(b'\x00shortcuts\x00\x05x\x00')
        with self.assertRaises(fai.Fail):
            fai.vdf_parse(b'\x00shortcuts\x00\x01AppName\x00abc\x00')

    def test_empty_file(self):
        self.assertEqual(fai.vdf_parse(b''), [])


class Shortcut(unittest.TestCase):
    exe = '/home/steamos/frameplayer/frameplayer.sh'

    def test_app_id_matches_frameplayer_install(self):
        # Same formula and vector as shortcut.rs: crc32("123456789") = 0xCBF43926.
        self.assertEqual(zlib.crc32(b'123456789') | 0x80000000, 0xCBF43926)
        self.assertEqual(fai.app_id('FramePlayer', self.exe),
                         zlib.crc32(f'"{self.exe}"FramePlayer'.encode()) | 0x80000000)

    def test_add_update_remove(self):
        doc = [['shortcuts', [['0', other('a')], ['1', other('b')]]]]
        aid, added = fai.upsert(doc, 'FramePlayer', self.exe, '/home/steamos/frameplayer', '/i.png')
        self.assertTrue(added)
        doc = fai.vdf_parse(fai.vdf_write(doc))
        e = fai.get(fai.get(doc, 'shortcuts'), '2')
        self.assertEqual(fai.get(e, 'appid'), fai.signed(aid))
        self.assertEqual(fai.get(e, 'Exe'), f'"{self.exe}"')
        self.assertEqual(fai.get(e, 'StartDir'), '"/home/steamos/frameplayer"')
        self.assertEqual(fai.get(fai.get(e, 'tags'), '0'), 'VR')

        # Update keeps what Steam and the user own, adds no duplicate.
        fai.put(e, 'LastPlayTime', 1700000000)
        fai.put(e, 'IsHidden', 1)
        _, added = fai.upsert(doc, 'FramePlayer', self.exe, '/home/steamos/frameplayer', '/i.png')
        self.assertFalse(added)
        sc = fai.get(doc, 'shortcuts')
        self.assertEqual(len(sc), 3)
        self.assertEqual(fai.get(e, 'LastPlayTime'), 1700000000)
        self.assertEqual(fai.get(e, 'IsHidden'), 1)
        self.assertEqual(len(fai.get(e, 'tags')), 1)

        # A copy renamed in Steam is still found by its Exe.
        _, added = fai.upsert(doc, 'FramePlayer Beta', self.exe, '/home/steamos/frameplayer', '')
        self.assertFalse(added)

        self.assertTrue(fai.remove_entries(doc, 'nope', self.exe))
        self.assertFalse(fai.remove_entries(doc, 'nope', self.exe))
        self.assertEqual([k for k, _ in sc], ['0', '1'])
        self.assertEqual(fai.get(fai.get(sc, '1'), 'AppName'), 'b')

    def test_test_copy_is_separate(self):
        doc = []
        fai.upsert(doc, 'FramePlayer', self.exe, '/x', '')
        _, added = fai.upsert(doc, 'FramePlayer (test)', '/home/steamos/Apps/frameplayer/frameplayer.sh',
                              '/y', '')
        self.assertTrue(added)
        self.assertEqual(len(fai.get(doc, 'shortcuts')), 2)

    def test_shortcut_file_is_backed_up_and_replaced(self):
        with tempfile.TemporaryDirectory() as cfg:
            path = os.path.join(cfg, 'shortcuts.vdf')
            with open(path, 'wb') as f:
                f.write(fai.vdf_write([['shortcuts', [['0', other('a')]]]]))
            fai.write_shortcut_file(cfg, lambda d: fai.upsert(d, 'X', '/x.sh', '/', ''))
            names = os.listdir(cfg)
            self.assertTrue(any(n.startswith('shortcuts.vdf.bak-') for n in names))
            with open(path, 'rb') as f:
                self.assertEqual(len(fai.get(fai.vdf_parse(f.read()), 'shortcuts')), 2)


class Unzip(unittest.TestCase):
    def make_zip(self, path, entries):
        with zipfile.ZipFile(path, 'w') as z:
            for name, mode, data in entries:
                info = zipfile.ZipInfo(name)
                info.create_system = 3
                info.external_attr = (mode << 16) | (0x10 if name.endswith('/') else 0)
                z.writestr(info, data)

    def test_keeps_exec_bits_and_strips_single_top_folder(self):
        with tempfile.TemporaryDirectory() as d:
            zp = os.path.join(d, 'a.zip')
            self.make_zip(zp, [('frameplayer/', 0o40755, b''),
                               ('frameplayer/frameplayer.sh', 0o100755, b'#!/bin/sh\n'),
                               ('frameplayer/README.txt', 0o100644, b'hi')])
            dest = os.path.join(d, 'out')
            fai.extract(zp, dest)
            self.assertTrue(os.stat(os.path.join(dest, 'frameplayer.sh')).st_mode & stat.S_IXUSR)
            self.assertFalse(os.stat(os.path.join(dest, 'README.txt')).st_mode & stat.S_IXUSR)

    def test_flat_zip_is_not_stripped(self):
        with tempfile.TemporaryDirectory() as d:
            zp = os.path.join(d, 'c.zip')
            self.make_zip(zp, [('chromium-xr.sh', 0o100755, b'x'), ('chromium/chrome', 0o100755, b'y')])
            dest = os.path.join(d, 'out')
            fai.extract(zp, dest)
            self.assertTrue(os.path.isfile(os.path.join(dest, 'chromium-xr.sh')))
            self.assertTrue(os.path.isfile(os.path.join(dest, 'chromium/chrome')))

    def test_rejects_paths_outside(self):
        with tempfile.TemporaryDirectory() as d:
            zp = os.path.join(d, 'bad.zip')
            self.make_zip(zp, [('ok.txt', 0o100644, b''), ('../evil.sh', 0o100755, b'')])
            with self.assertRaises(fai.Fail):
                fai.extract(zp, os.path.join(d, 'out'))
            self.assertFalse(os.path.exists(os.path.join(d, 'evil.sh')))

    def test_swap_keeps_previous_version(self):
        with tempfile.TemporaryDirectory() as d:
            zp = os.path.join(d, 'a.zip')
            self.make_zip(zp, [('frameplayer/frameplayer.sh', 0o100755, b'v2')])
            dest = os.path.join(d, 'frameplayer')
            os.makedirs(dest)
            with open(os.path.join(dest, 'frameplayer.sh'), 'w') as f:
                f.write('v1')
            fai.install_files(fai.APPS['frameplayer'], zp, dest)
            for path, want in ((dest, 'v2'), (dest + '.old', 'v1')):
                with open(os.path.join(path, 'frameplayer.sh')) as f:
                    self.assertEqual(f.read(), want)
            self.assertFalse(os.path.exists(dest + '.new'))


if __name__ == '__main__':
    unittest.main()
