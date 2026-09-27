#!/usr/bin/env python
"""PE 探查小工具:列出导入(带 IAT 虚拟地址)、把 VA 反查成文件偏移、读字符串。

用法:
  python tools/pe.py imports  <path> [模块名子串]
  python tools/pe.py str      <path> <va> [<va> ...]
  python tools/pe.py bytes    <path> <va> [长度]
"""
import struct
import sys


def load(path):
    data = open(path, 'rb').read()
    u16 = lambda o: struct.unpack_from('<H', data, o)[0]
    u32 = lambda o: struct.unpack_from('<I', data, o)[0]

    pe = u32(0x3C)
    machine = u16(pe + 4)
    nsec = u16(pe + 6)
    sizeofopt = u16(pe + 20)
    magic = u16(pe + 24)
    imagebase = u32(pe + 52) if magic == 0x10B else struct.unpack_from('<Q', data, pe + 52)[0]
    ddir = pe + 24 + (96 if magic == 0x10B else 112)

    dirs = {}
    for i in range(min(u32(pe + 22), 16)):
        rva, size = u32(ddir + i * 8), u32(ddir + i * 8 + 4)
        if rva:
            dirs[i] = (rva, size)

    secs = []
    o = pe + 24 + sizeofopt
    for _ in range(nsec):
        name = data[o:o + 8].rstrip(b'\0').decode('latin1')
        va, vsz, raw, rawsz = u32(o + 12), u32(o + 8), u32(o + 20), u32(o + 16)
        secs.append((name, va, max(vsz, rawsz), raw))
        o += 40

    def to_off(rva):
        for _n, va, sz, raw in secs:
            if va <= rva < va + sz:
                return raw + (rva - va)
        return None

    return dict(data=data, machine=machine, base=imagebase, dirs=dirs, off=to_off)


def imports(pe, want):
    if 1 not in pe['dirs']:
        print('(无导入表)')
        return
    data, base, off = pe['data'], pe['base'], pe['off']
    o = off(pe['dirs'][1][0])
    while True:
        name_rva = struct.unpack_from('<I', data, o)[0]
        orig = struct.unpack_from('<I', data, o + 16)[0]
        ft = struct.unpack_from('<I', data, o + 20)[0]
        if name_rva == 0 and orig == 0:
            break
        no = off(name_rva)
        raw = data[no:no + 256].split(b'\0\0')[0]
        if raw[1::2] == b'\0' * (len(raw[1::2])) and len(raw) > 4:
            mod = raw.decode('utf-16-le', 'replace')
        else:
            mod = data[no:data.index(b'\0', no)].decode('latin1')
        src = ft or orig
        so = off(src)
        entries, i = [], 0
        while True:
            v = struct.unpack_from('<I', data, so + i * 4)[0]
            if v == 0:
                break
            if v & 0x80000000:
                nm = 'ordinal %d' % (v & 0xFFFF)
            else:
                io = off(v & 0x7FFFFFFF)
                nm = data[io + 2:data.index(b'\0', io + 2)].decode('latin1')
            entries.append('%#x  %s' % (base + src + i * 4, nm))
            i += 1
        if want in mod.lower():
            print(mod)
            for e in entries:
                print('   ', e)
        o += 20


def read_str(data, pe, va, as_utf16):
    o = pe['off'](va - pe['base'])
    if o is None:
        return '<不可映射>'
    if as_utf16:
        units = []
        for i in range(0, 512, 2):
            c = struct.unpack_from('<H', data, o + i)[0]
            if c == 0:
                break
            units.append(c)
        return ''.join(chr(u) for u in units)
    return data[o:data.index(b'\0', o)].decode('latin1')


def main():
    cmd, path = sys.argv[1], sys.argv[2]
    pe = load(path)
    data = pe['data']
    if cmd == 'imports':
        imports(pe, (sys.argv[3] if len(sys.argv) > 3 else '').lower())
    elif cmd == 'str':
        for spec in sys.argv[3:]:
            va = int(spec, 0)
            print('%#x  A=%r  W=%r' % (
                va, read_str(data, pe, va, False), read_str(data, pe, va, True)))
    elif cmd == 'bytes':
        va = int(sys.argv[3], 0)
        n = int(sys.argv[4], 0) if len(sys.argv) > 4 else 32
        o = pe['off'](va - pe['base'])
        blob = data[o:o + n]
        for i in range(0, len(blob), 16):
            print('%#x  %-48s %s' % (va + i,
                                     ' '.join('%02x' % b for b in blob[i:i + 16]),
                                     ''.join(chr(b) if 32 <= b < 127 else '.' for b in blob[i:i + 16])))
        for i in range(0, len(blob) - 3, 4):
            v = struct.unpack_from('<I', blob, i)[0]
            if pe['base'] <= v < pe['base'] + 0x0C000000:
                print('  ptr@%#x -> %#x' % (va + i, v))
    else:
        print(__doc__)


if __name__ == '__main__':
    main()
