# PS3-PKG-Module

Extrai e cria arquivos `.pkg` do PS3 (e PSP).

```bash
pip install pycryptodome       # opcional, necessario para PKGs finalized
python build_deps.py            # opcional, acelera extracao de PKGs non-finalized
```

---

## Extração (`main.py`)

Extrai PKGs de URL ou arquivo local.

```bash
python main.py "http://site.com/game.pkg"                # de URL
python main.py Sonic_the_Hedgehog_2.pkg                  # de arquivo local
python main.py Sonic_the_Hedgehog_2.pkg --sfo            # extrai apenas PARAM.SFO
python main.py Sonic_the_Hedgehog_2.pkg --content-id     # so mostra o Content ID
python main.py Sonic_the_Hedgehog_2.pkg -o minha_pasta   # diretorio de saida
```

| Flag | Descrição |
|------|-----------|
| (nenhuma) | Extrai **todos** os arquivos (padrão) |
| `--sfo` | Extrai apenas `PARAM.SFO` |
| `--eboot` | Extrai apenas `EBOOT.BIN` |
| `--pic1` / `--pic0` / `--icon` | Extrai a imagem correspondente |
| `--path CAMINHO` | Extrai um arquivo específico (ex: `--path /USRDIR/EBOOT.BIN`) |
| `--content-id` | Exibe o Content ID e sai (não extrai nada) |
| `-o DIR` | Diretório de saída (padrão: `PS3/`) |

---

## Construção de PKG (`pkg_builder.py`)

Monta um `.pkg` a partir de um diretório. Dois modos:

| Modo | Uso |
|------|-----|
| **custom** (padrão) | Paths com prefixo `../../` para CFW |
| **retail** (`--retail`) | Paths relativos (ex: `USRDIR/EBOOT.BIN`) |

### Modo custom

O diretório espelha o HDD do PS3. Os paths recebem prefixo `../../` automaticamente.

```
custom/
└── dev_hdd0/
    └── game/
        └── NPUB30443/
            ├── USRDIR/EBOOT.BIN
            └── PARAM.SFO
```

```bash
python pkg_builder.py                         # usa ./custom/
python pkg_builder.py minha_pasta/            # diretorio customizado
```

### Modo retail

O diretório contém a estrutura extraída pelo `main.py`. Content type é detectado automaticamente (GameExec se `USRDIR/EBOOT.BIN` existir, senão GameData).

```
extracao/
├── PARAM.SFO
├── ICON0.PNG
├── USRDIR/
│   └── EBOOT.BIN
└── TROPDIR/NPWR00001_00/TROPHY.TRP
```

```bash
python pkg_builder.py extracao/ --retail -c "UP0177-NPUB30443_00-SVCSONIC2XXXXXXX"
python pkg_builder.py extracao/ --retail -c "UP0177-NPUB30443_00-SVCSONIC2XXXXXXX" --rap game.rap
```

### Opções

| Flag | Descrição |
|------|-----------|
| `--retail` | Modo retail (sem prefixo `../../`) |
| `--rap ARQUIVO.rap` | Arquivo `.rap` de licença (16 bytes) |
| `-o ARQUIVO` | Nome do `.pkg` de saída |
| `-c ID` | Content ID |

### Exemplos

```bash
# Custom
python pkg_builder.py -o meu_installer.pkg

# Retail
python pkg_builder.py extracao/ --retail -c "UP0177-NPUB30443_00-SVCSONIC2XXXXXXX"

# Retail com RAP
python pkg_builder.py extracao/ --retail -c "UP0177-NPUB30443_00-SVCSONIC2XXXXXXX" --rap game.rap -o saida.pkg
```

Saída:
```
PKG criado: saida.pkg (35433712 bytes)
Content ID: UP0177-NPUB30443_00-SVCSONIC2XXXXXXX
Modo: retail
RAP copiado: saida.rap
```

---

## SFO (`sfo_maker.py` / `sfo_editor.py`)

Cria e edita arquivos `PARAM.SFO` do PS3 via CLI ou como módulo Python.

### CLI — Criar SFO

```bash
# Cria PARAM.SFO com valores padrão
python sfo_maker.py

# Cria com valores customizados
python sfo_maker.py -o game.sfo --title "Meu Jogo" --title-id "GAME00001" --category "HG"
```

| Flag | Padrão (valor real) | Descrição |
|------|---------------------|-----------|
| `-o ARQUIVO` | `PARAM.SFO` | Arquivo de saída |
| `--title` | `The Simpsons - Road Rage` | Título do jogo |
| `--title-id` | `SLUS20305` | Title ID |
| `--category` | `2P` | Categoria (HG, DG, 2P, etc) |
| `--bootable` | `1` | Flag bootável (0 ou 1) |
| `--attribute` | `0` | Atributo |
| `--parental-level` | `0` | Nível de controle parental |
| `--ps3-system-ver` | `03.4000` | Versão mínima do sistema |
| `--region-deny` | `0` | Flags de negação regional |

### CLI — Editar/Visualizar SFO

```bash
# Visualizar conteúdo
python sfo_editor.py PARAM.SFO

# Editar campos (faz backup .bak automaticamente)
python sfo_editor.py PARAM.SFO --title "Novo Título" --title-id "BLUS99999"
python sfo_editor.py PARAM.SFO --category "DG" --bootable 0
python sfo_editor.py PARAM.SFO --set TITLE="Outro" --set BOOTABLE=1

# Salvar em outro arquivo (sem modificar o original)
python sfo_editor.py PARAM.SFO -o editado.sfo --title "Editado"
```
---

## Aceleração C (`build_deps.py`)

Compila a extensão nativa para acelerar a descriptografia de PKGs non-finalized.

```bash
python build_deps.py              # compila
python build_deps.py --clean      # remove binarios compilados
```

Se a extensão C não estiver disponível, o fallback em Python puro é usado automaticamente.

---

## Licença

MIT
