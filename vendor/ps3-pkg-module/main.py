#!/usr/bin/env python3
"""
PS3 PKG Extractor - Extrai arquivos .pkg do PS3 (local ou URL via Range requests)
"""
import os, sys, struct, hashlib, argparse, re
from abc import ABC, abstractmethod

try:
    from Crypto.Cipher import AES
except ImportError:
    AES = None

PKG_MAGIC = b'\x7FPKG'

PS3_AES_KEY = bytes([
    0x2E, 0x7B, 0x71, 0xD7, 0xC9, 0xC9, 0xA1, 0x4E,
    0xA3, 0x22, 0x1F, 0x18, 0x88, 0x28, 0xB8, 0xF8
])

PSP_AES_KEY = bytes([
    0x07, 0xF2, 0xC6, 0x82, 0x90, 0xB5, 0x0D, 0x2C,
    0x33, 0x81, 0x8D, 0x70, 0x9B, 0x60, 0xE6, 0x2B
])

MAX_CACHE_SIZE = 0x8000000  # 128 MB

HAVE_PKGCRYPT = False
_self_dir = os.path.dirname(os.path.abspath(__file__))
for _p in [_self_dir, os.path.join(_self_dir, '..', 'pypkg')]:
    if os.path.isdir(_p):
        sys.path.insert(0, _p)
try:
    import pkgcrypt
    HAVE_PKGCRYPT = True
except ImportError:
    pass


def _u32(d, o):
    return struct.unpack('>I', d[o:o+4])[0]


def _u64(d, o):
    return struct.unpack('>Q', d[o:o+8])[0]


def _inc_ctr(ctr, n=1):
    c = bytearray(ctr)
    carry = n
    for i in range(15, -1, -1):
        val = c[i] + carry
        c[i] = val & 0xFF
        carry = val >> 8
        if carry == 0:
            break
    return bytes(c)


def _make_key64(digest):
    k = bytearray(64)
    k[0:8] = digest[0:8]
    k[8:16] = digest[0:8]
    k[16:24] = digest[8:16]
    k[24:32] = digest[8:16]
    return k


def _decrypt_finalized(enc, iv, aes_key, offset=0):
    block_start = offset // 16
    nb = (len(enc) + 15) // 16
    buf = bytearray(nb * 16)
    ctr = bytearray(_inc_ctr(iv, block_start))
    for i in range(nb):
        off = i * 16
        buf[off:off+16] = ctr
        for j in range(15, -1, -1):
            ctr[j] = (ctr[j] + 1) & 0xFF
            if ctr[j] != 0:
                break
    ks = AES.new(aes_key, AES.MODE_ECB).encrypt(bytes(buf))[:len(enc)]
    return bytes(a ^ b for a, b in zip(enc, ks))


def _decrypt_non_finalized(enc, digest, offset=0):
    block_start = offset // 16
    k = _make_key64(digest)
    ctr_val = _u64(bytes(k), 0x38) + block_start
    k[0x38:0x40] = struct.pack('>Q', ctr_val)

    if HAVE_PKGCRYPT and offset == 0:
        return pkgcrypt.pkgcrypt(bytes(k), enc, len(enc))

    out = bytearray(len(enc))
    for i in range(0, len(enc), 16):
        chunk = min(16, len(enc) - i)
        h = hashlib.sha1(bytes(k)).digest()
        for j in range(chunk):
            out[i + j] = enc[i + j] ^ h[j]
        ctr_val += 1
        k[0x38:0x40] = struct.pack('>Q', ctr_val)
    return bytes(out)


# --- Readers ---

class PkgReader(ABC):
    @abstractmethod
    def read_at(self, offset, size):
        pass

    @abstractmethod
    def size(self):
        pass

    def close(self):
        pass


# --- Readers ---

def _is_manifest(src):
    s = str(src)
    if s.endswith('.xml'):
        return True
    if s.startswith(('http://', 'https://')):
        # Heuristica: URLs de manifesto costumam conter "manifest"
        return 'manifest' in s.lower() or s.lower().endswith('.xml')
    if os.path.exists(s):
        try:
            with open(s, 'rb') as f:
                head = f.read(64)
            return b'<hfs_manifest' in head or b'hfs_manifest' in head
        except OSError:
            return False
    return False


class ManifestPkgReader(PkgReader):
    """Le um PKG multipart a partir de um manifesto hfs_manifest (XML local ou URL).
    Mapeia offsets virtuais [0, total_size) para as partes via Range requests,
    reaproveitando a mesma logica do RemotePkgReader."""
    def __init__(self, manifest_src):
        self.pieces = []
        self._total_size = 0
        self._file_name = None
        self._load_manifest(manifest_src)

    def _fetch_text(self, src):
        if str(src).startswith(('http://', 'https://')):
            from urllib.request import Request, urlopen
            req = Request(str(src))
            with urlopen(req, timeout=30) as r:
                return r.read().decode('utf-8', errors='replace')
        with open(str(src), 'r', encoding='utf-8', errors='replace') as f:
            return f.read()

    def _load_manifest(self, src):
        xml = self._fetch_text(src)
        m_fn = re.search(r'<file_name>\s*(.*?)\s*</file_name>', xml)
        if m_fn:
            self._file_name = m_fn.group(1).strip()
        m_sz = re.search(r'<file_size>\s*(\d+)\s*</file_size>', xml)
        if m_sz:
            self._total_size = int(m_sz.group(1))
        for m in re.finditer(
            r'<pieces\s+[^>]*?file_size="(\d+)"[^>]*?index="(\d+)"[^>]*?url="([^"]+)"',
            xml
        ):
            fs, idx, url = int(m.group(1)), int(m.group(2)), m.group(3)
            self.pieces.append((idx, url, fs))
        # fallback: aceita ordem de atributos diferente
        if not self.pieces:
            for m in re.finditer(r'<pieces\s+([^>]+)>', xml):
                attrs = m.group(1)
                fs_m = re.search(r'file_size="(\d+)"', attrs)
                idx_m = re.search(r'index="(\d+)"', attrs)
                url_m = re.search(r'url="([^"]+)"', attrs)
                if fs_m and idx_m and url_m:
                    self.pieces.append((int(idx_m.group(1)), url_m.group(1), int(fs_m.group(1))))
        if not self.pieces:
            raise ValueError('Manifesto invalido: nenhuma <pieces> encontrada')
        self.pieces.sort(key=lambda p: p[0])
        if self._total_size == 0:
            self._total_size = sum(p[2] for p in self.pieces)

    def size(self):
        return self._total_size

    def _piece_ranges(self, offset, size):
        """Mapeia [offset, offset+size) para (url, start, length) por parte."""
        out = []
        pos = 0
        for _idx, url, fs in self.pieces:
            if size == 0:
                break
            if pos + fs <= offset:
                pos += fs
                continue
            if pos >= offset + size:
                break
            local_start = max(0, offset - pos)
            local_end = min(fs, offset + size - pos)
            out.append((url, local_start, local_end - local_start))
            pos += fs
        return out

    def read_at(self, offset, size):
        from urllib.request import Request, urlopen
        from urllib.error import HTTPError, URLError
        if size == 0 or offset >= self._total_size:
            return b''
        size = min(size, self._total_size - offset)
        chunks = []
        for url, start, length in self._piece_ranges(offset, size):
            req = Request(url)
            req.add_header('Range', f'bytes={start}-{start + length - 1}')
            try:
                with urlopen(req, timeout=60) as r:
                    if r.status == 206:
                        chunks.append(r.read())
                    else:
                        raise RuntimeError(f'Server does not support Range requests (HTTP {r.status})')
            except HTTPError as e:
                raise RuntimeError(f'HTTP {e.code} ao acessar {url}')
            except URLError as e:
                raise RuntimeError(f'Falha de conexao: {e.reason}')
        return b''.join(chunks)


class LocalPkgReader(PkgReader):
    def __init__(self, path):
        self.path = path
        self._size = os.path.getsize(path)
        self._fh = None

    def size(self):
        return self._size

    def read_at(self, offset, size):
        if self._fh is None:
            self._fh = open(self.path, 'rb')
        self._fh.seek(offset)
        return self._fh.read(size)

    def close(self):
        if self._fh:
            self._fh.close()


class RemotePkgReader(PkgReader):
    def __init__(self, url):
        self.url = url
        self._size = self._fetch_size()

    def _fetch_size(self):
        from urllib.request import Request, urlopen
        req = Request(self.url, method='HEAD')
        with urlopen(req, timeout=30) as r:
            return int(r.headers['Content-Length'])

    def size(self):
        return self._size

    def read_at(self, offset, size):
        from urllib.request import Request, urlopen
        from urllib.error import HTTPError, URLError
        if size == 0:
            return b''
        req = Request(self.url)
        req.add_header('Range', f'bytes={offset}-{offset + size - 1}')
        try:
            with urlopen(req, timeout=60) as r:
                if r.status == 206:
                    return r.read()
                raise RuntimeError(f'Server does not support Range requests (HTTP {r.status})')
        except HTTPError as e:
            raise RuntimeError(f'HTTP {e.code} ao acessar {self.url}')
        except URLError as e:
            raise RuntimeError(f'Falha de conexao: {e.reason}')


# --- PKG ---

CHUNK_SIZE = 4 * 1024 * 1024  # 4 MB streaming chunk


class PS3PKG:
    def __init__(self, source):
        if isinstance(source, PkgReader):
            self.reader = source
        elif _is_manifest(source):
            self.reader = ManifestPkgReader(str(source))
        elif str(source).startswith(('http://', 'https://')):
            self.reader = RemotePkgReader(str(source))
        else:
            self.reader = LocalPkgReader(str(source))
        self._parse_header()
        self._alt_key = None
        self._digest = None
        self._digest_probed = False

    def _parse_header(self):
        d = self.reader.read_at(0, 0xC0)
        if d[0:4] != PKG_MAGIC:
            raise ValueError('Not a valid PKG file')
        self.pkg_revision = struct.unpack('>H', d[4:6])[0]
        self.pkg_type = struct.unpack('>H', d[6:8])[0]
        self.meta_off = _u32(d, 0x08)
        self.meta_cnt = _u32(d, 0x0C)
        self.meta_sz = _u32(d, 0x10)
        self.item_cnt = _u32(d, 0x14)
        self.total_size = _u64(d, 0x18)
        self.data_off = _u64(d, 0x20)
        self.data_sz = _u64(d, 0x28)
        self.content_id = d[0x30:0x60].rstrip(b'\x00').decode('ascii', errors='replace')
        self.digest = d[0x60:0x70]
        self.iv = d[0x70:0x80]
        self.header_digest = d[0x80:0xC0]

        if self.pkg_type == 1:
            self.aes_key = PS3_AES_KEY; self.platform = 'PS3'
        elif self.pkg_type == 2:
            self.aes_key = PSP_AES_KEY; self.platform = 'PSP'
        else:
            raise ValueError(f'Unknown PKG type: {self.pkg_type}')
        self.is_finalized = self.pkg_revision == 0x8000

    def _probe_digest(self):
        if self._digest_probed:
            return
        raw = self.reader.read_at(self.data_off, 32)
        probe = _decrypt_non_finalized(raw, self.header_digest)
        ts = _u32(probe, 0)
        if ts == 0 or ts > self.data_sz or ts % 32 != 0:
            self._alt_key = self.digest
            self._digest = self.digest
        else:
            self._digest = self.header_digest
        self._digest_probed = True

    def _read_decrypted(self, offset, size):
        if not self.is_finalized:
            self._probe_digest()
        enc = self.reader.read_at(self.data_off + offset, size)
        if self.is_finalized:
            return _decrypt_finalized(enc, self.iv, self.aes_key, offset)
        return _decrypt_non_finalized(enc, self._digest, offset)

    def _get_file_table(self):
        hdr = self._read_decrypted(0, 16)
        ffo = _u32(hdr, 12)
        if ffo > self.data_sz:
            raise ValueError(f'Invalid file table offset: 0x{ffo:x} > data_sz 0x{self.data_sz:x}')
        return self._read_decrypted(0, ffo)

    def _extract_filtered(self, out_dir, file_filter, flat=False):
        os.makedirs(out_dir, exist_ok=True)
        ft = self._get_file_table()
        num_files = _u32(ft, 0) // 32
        extracted = 0

        for i in range(num_files):
            eo = i * 32
            no = _u32(ft, eo); ns = _u32(ft, eo + 4)
            fo = _u32(ft, eo + 12); fs = _u32(ft, eo + 20)
            ct = ft[eo + 24]; ft_type = ft[eo + 27]
            is_dir = (ft_type == 0x04) and (fs == 0)

            if ct == 0x90:
                name = ft[no:no + ns].rstrip(b'\x00').decode('ascii', errors='replace')
            else:
                nd = self._read_decrypted(no, ns)
                name = nd.rstrip(b'\x00').decode('ascii', errors='replace')

            name = name.replace('/', os.sep)
            parts = name.split(os.sep)
            while parts and parts[0] == '..':
                parts.pop(0)
            name = os.sep.join(parts)

            if is_dir:
                if not flat:
                    os.makedirs(os.path.join(out_dir, name), exist_ok=True)
            elif file_filter(name):
                if flat:
                    fp = os.path.join(out_dir, os.path.basename(name))
                else:
                    fp = os.path.join(out_dir, name)
                    os.makedirs(os.path.dirname(fp), exist_ok=True)

                if ct == 0x90:
                    with open(fp, 'wb') as f:
                        f.write(ft[fo:fo + fs])
                else:
                    with open(fp, 'wb') as f:
                        off = fo
                        remain = fs
                        while remain > 0:
                            chunk = min(CHUNK_SIZE, remain)
                            f.write(self._read_decrypted(off, chunk))
                            off += chunk
                            remain -= chunk
                extracted += 1

        return extracted

    def extract(self, out_dir):
        return self._extract_filtered(out_dir, lambda _: True)

    def extract_sfo(self, out_dir):
        return self._extract_filtered(out_dir, lambda n: os.path.basename(n) == 'PARAM.SFO', flat=True)

    def extract_eboot(self, out_dir):
        return self._extract_filtered(out_dir, lambda n: os.path.basename(n) == 'EBOOT.BIN', flat=True)

    def extract_pic1(self, out_dir):
        return self._extract_filtered(out_dir, lambda n: os.path.basename(n) == 'PIC1.PNG', flat=True)

    def extract_pic0(self, out_dir):
        return self._extract_filtered(out_dir, lambda n: os.path.basename(n) == 'PIC0.PNG', flat=True)

    def extract_icon(self, out_dir):
        return self._extract_filtered(out_dir, lambda n: os.path.basename(n) == 'ICON0.PNG', flat=True)

    def _list_files(self):
        ft = self._get_file_table()
        num_files = _u32(ft, 0) // 32
        names = []
        for i in range(num_files):
            eo = i * 32
            no = _u32(ft, eo); ns = _u32(ft, eo + 4)
            fo = _u32(ft, eo + 12); fs = _u32(ft, eo + 20)
            ct = ft[eo + 24]; ft_type = ft[eo + 27]
            is_dir = (ft_type == 0x04) and (fs == 0)
            if is_dir:
                continue
            if ct == 0x90:
                name = ft[no:no + ns].rstrip(b'\x00').decode('ascii', errors='replace')
            else:
                nd = self._read_decrypted(no, ns)
                name = nd.rstrip(b'\x00').decode('ascii', errors='replace')
            names.append(name)
        return names

    def list_files(self):
        """Lista recursivamente todas as pastas e arquivos do PKG numa estrutura
        hierarquica. Retorna um dicionario no formato:
            {
                'name': '<raiz>',
                'type': 'dir',
                'children': [ {'name', 'type', 'children'? | 'size'?}, ... ]
            }
        Pastas derivam de entradas is_dir ou sao inferidas dos caminhos dos
        arquivos (sempem recursivas)."""
        ft = self._get_file_table()
        num_files = _u32(ft, 0) // 32
        root = {'name': '', 'type': 'dir', 'children': {}}

        def insert(path, is_dir=False, size=0):
            parts = [p for p in path.replace('\\', '/').split('/') if p]
            node = root
            for i, p in enumerate(parts):
                last = i == len(parts) - 1
                if last and not is_dir:
                    child = node['children'].get(p)
                    if not child or child['type'] != 'file':
                        child = {'name': p, 'type': 'file', 'size': size}
                        node['children'][p] = child
                    else:
                        child['size'] = size
                else:
                    child = node['children'].get(p)
                    if not child:
                        child = {'name': p, 'type': 'dir', 'children': {}}
                        node['children'][p] = child
                    elif child['type'] != 'dir':
                        child = {'name': p, 'type': 'dir', 'children': {}}
                        node['children'][p] = child
                    else:
                        child.setdefault('children', {})
                    node = child

        for i in range(num_files):
            eo = i * 32
            no = _u32(ft, eo); ns = _u32(ft, eo + 4)
            fo = _u32(ft, eo + 12); fs = _u32(ft, eo + 20)
            ct = ft[eo + 24]; ft_type = ft[eo + 27]
            is_dir = (ft_type == 0x04) and (fs == 0)
            if ct == 0x90:
                name = ft[no:no + ns].rstrip(b'\x00').decode('ascii', errors='replace')
            else:
                nd = self._read_decrypted(no, ns)
                name = nd.rstrip(b'\x00').decode('ascii', errors='replace')
            name = name.replace('\\', '/')
            while name.startswith('/'):
                name = name[1:]
            parts = [p for p in name.split('/') if p]
            while parts and parts[0] == '..':
                parts.pop(0)
            name = '/'.join(parts)
            insert(name.lstrip('/'), is_dir=is_dir, size=fs)

        def to_list(node):
            children = node.get('children', {})
            out = []
            for name in sorted(children.keys(), key=lambda s: (s.lower(), s)):
                c = children[name]
                entry = {'name': c['name'], 'type': c['type']}
                if c['type'] == 'dir':
                    entry['children'] = to_list(c)
                else:
                    entry['size'] = c.get('size', 0)
                out.append(entry)
            return out

        root['name'] = self.content_id
        return {'name': root['name'], 'type': 'dir', 'children': to_list(root)}

    def extract_path(self, out_dir, pkg_path):
        orig = pkg_path
        drive, tail = os.path.splitdrive(pkg_path)
        if drive:
            parts = tail.replace(os.sep, '/').strip('/').split('/')
            files = self._list_files()
            for i in range(len(parts)):
                candidate = '/'.join(parts[i:])
                if candidate in files:
                    pkg_path = candidate
                    break
            else:
                raise ValueError(f'Arquivo "{orig}" nao encontrado no PKG')
        pkg_path = pkg_path.lstrip('/').replace('/', os.sep)
        n = self._extract_filtered(out_dir, lambda name: name == pkg_path, flat=True)
        if n == 0:
            raise ValueError(f'Arquivo "{orig}" nao encontrado no PKG')
        return n


# --- CLI ---

def _print_file_tree(node):
    """Imprime a estrutura de pastas/arquivos em arvore ASCII recursiva."""
    def fmt_size(n):
        for unit in ['B', 'KB', 'MB', 'GB']:
            if n < 1024:
                return f'{n:.1f}{unit}' if unit != 'B' else f'{n}{unit}'
            n /= 1024
        return f'{n:.1f}TB'

    def render(entries, prefix=''):
        entries = list(entries)
        for i, e in enumerate(entries):
            last = i == len(entries) - 1
            connector = '└── ' if last else '├── '
            suffix = '/' if e['type'] == 'dir' else f'  ({fmt_size(e.get("size", 0))})'
            print(f'{prefix}{connector}{e["name"]}{suffix}')
            if e['type'] == 'dir':
                ext = '    ' if last else '│   '
                render(e.get('children', []), prefix + ext)

    print(f'{node["name"]}/')
    render(node.get('children', []))


def main():
    parser = argparse.ArgumentParser(description='PS3 PKG Extractor')
    parser.add_argument('source', nargs='?', help='Caminho ou URL do .pkg')
    parser.add_argument('-o', '--output', default='PS3', help='Diretorio de saida (padrao: PS3)')
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--full', action='store_true', help='Extrai todos os arquivos (padrao)')
    mode.add_argument('--sfo', action='store_true', help='Extrai apenas PARAM.SFO')
    mode.add_argument('--eboot', action='store_true', help='Extrai apenas EBOOT.BIN')
    mode.add_argument('--pic1', action='store_true', help='Extrai apenas PIC1.PNG')
    mode.add_argument('--pic0', action='store_true', help='Extrai apenas PIC0.PNG')
    mode.add_argument('--icon', action='store_true', help='Extrai apenas ICON0.PNG')
    mode.add_argument('--path', metavar='CAMINHO', help='Extrai arquivo especifico do PKG (ex: /USRDIR/EBOOT.BIN)')
    mode.add_argument('--content-id', action='store_true', help='Apenas exibe o Content ID sem extrair nada')
    mode.add_argument('--list-files', action='store_true', help='Lista recursivamente pastas e arquivos do PKG sem extrair')
    args = parser.parse_args()

    src = args.source
    if not src:
        src = os.path.join(os.path.dirname(__file__), 'Sonic_the_Hedgehog_2.pkg')
        if not os.path.exists(src):
            parser.print_usage()
            print('main.py: error: informe um caminho ou URL do .pkg', file=sys.stderr)
            sys.exit(1)

    if not os.path.exists(src) and not str(src).startswith(('http://', 'https://')) and not _is_manifest(src):
        print(f'[ERRO] Arquivo nao encontrado: {src}', file=sys.stderr)
        sys.exit(1)

    try:
        pkg = PS3PKG(src)
    except Exception as e:
        print(f'[ERRO] {e}', file=sys.stderr)
        sys.exit(1)

    print(f'Platform: {pkg.platform}')
    print(f'Finalized: {pkg.is_finalized}')
    print(f'Content ID: {pkg.content_id}')
    print(f'Items: {pkg.item_cnt}')
    print(f'Total: {pkg.total_size} bytes')
    print(f'Source: {src}')

    if args.content_id:
        return

    if args.list_files:
        try:
            tree = pkg.list_files()
            _print_file_tree(tree)
        except Exception as e:
            print(f'[ERRO] Falha ao listar arquivos: {e}', file=sys.stderr)
            sys.exit(1)
        finally:
            pkg.reader.close()
        return

    out = os.path.join(os.getcwd(), args.output)
    try:
        if args.sfo:
            n = pkg.extract_sfo(out)
            label = 'PARAM.SFO'
        elif args.eboot:
            n = pkg.extract_eboot(out)
            label = 'EBOOT.BIN'
        elif args.pic1:
            n = pkg.extract_pic1(out)
            label = 'PIC1.PNG'
        elif args.pic0:
            n = pkg.extract_pic0(out)
            label = 'PIC0.PNG'
        elif args.icon:
            n = pkg.extract_icon(out)
            label = 'ICON0.PNG'
        elif args.path:
            n = pkg.extract_path(out, args.path)
            label = args.path
        else:
            n = pkg.extract(out)
            label = 'todos'
        print(f'Extraidos {n} arquivo(s) de {label} para {out}')
    except Exception as e:
        print(f'[ERRO] Falha na extracao: {e}', file=sys.stderr)
        sys.exit(1)
    finally:
        pkg.reader.close()


if __name__ == '__main__':
    main()
