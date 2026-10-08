# 🧠 Storage Engine — Base do Banco de Dados

## 📌 Visão Geral

Até o momento, foi construída a base do **Storage Engine** de um banco de dados — responsável por:

- gerenciar páginas em memória
- decidir quais páginas permanecem no cache
- preparar a comunicação com o disco

Essa camada representa o núcleo de sistemas de banco de dados modernos.

---

# 🧱 Componentes Implementados

## 📄 Page (Unidade básica de dados)

A `Page` representa um bloco fixo de dados carregado na memória.

### 🔹 Responsabilidades

- armazenar dados brutos (`Vec<u8>`)
- identificar qual página do disco está carregada (`page_id`)
- controlar uso (`pin_count`)
- indicar se precisa ser persistida (`is_dirty`)

### 🔹 Estrutura

```rust
pub struct Page {
    page_id: Option<PageId>,
    data: Vec<u8>,
    pin_count: usize,
    is_dirty: bool,
}
```

### Conceitos importantes

page_id = None → frame livre
pin_count > 0 → página em uso (não pode ser removida)
is_dirty = true → precisa ser escrita no disco antes de sair

## 🧠 Buffer Pool (Memória gerenciada manualmente)

O Buffer Pool é uma área de memória que armazena páginas do banco.

🔹 Estrutura
pub struct BufferPoolManager {
    pages: Vec<Page>,
    free_list: Vec<FrameId>,
    pool_size: usize,
}
🔹 Responsabilidades
manter páginas em memória
controlar frames livres
servir como cache entre disco e aplicação
🔑 Conceitos importantes
Frames
cada posição em pages é um frame
um frame pode conter uma página ou estar vazio
Free List
lista de frames nunca usados ou liberados
sempre utilizada antes de qualquer política de substituição
🚀 LRU-K Replacer (Política de substituição)

Foi implementado um algoritmo LRU-K, responsável por decidir qual página deve sair da memória quando necessário.

🧠 Ideia principal

Diferente do LRU simples, o LRU-K considera K acessos anteriores para tomar decisão.

🔹 Estrutura
pub struct LRUKReplacer {
    k: usize,
    current_timestamp: usize,
    evictable: HashSet<FrameId>,
    history: HashMap<FrameId, VecDeque<usize>>,
}
🔹 Responsabilidades
registrar acessos às páginas
manter histórico de uso
identificar páginas elegíveis para remoção
selecionar vítima para substituição
📊 Estratégia de decisão

Para cada página:

Caso 1: menos de K acessos
prioridade máxima para remoção
geralmente páginas acessadas poucas vezes
Caso 2: pelo menos K acessos

Cálculo da distância:

distância = tempo atual - K-ésimo último acesso
remove a página com maior distância
⚠️ Regras importantes
páginas com pin_count > 0 não são elegíveis
em caso de empate → menor frame_id
ao remover:
página é retirada do histórico
deixa de ser evictable
🔗 Como tudo se conecta

Mesmo sem integração completa, o fluxo já está definido:

Requisição de página
        ↓
Buffer Pool
        ↓
(verifica memória)
        ↓
[ futuro ] Page Table (hash)
        ↓
(se não estiver na memória)
        ↓
LRU-K escolhe vítima
        ↓
(substituição de página)
🧠 Decisões de design
✔️ Controle manual de memória
o banco gerencia explicitamente quais páginas ficam em RAM
não depende do sistema operacional
✔️ Separação de responsabilidades
Page → estado da página
BufferPool → gerenciamento de memória
LRU-K → política de substituição
✔️ Estruturas simples e eficientes
Vec para armazenamento contínuo
HashMap para lookup
VecDeque para histórico de acessos
📌 Próximos passos

Para evoluir o banco:

🔜 Page Table (Extendible Hash)
mapear page_id → frame_id
🔜 Buffer Pool completo
fetch_page
unpin_page
new_page
flush_page
🔜 Integração com Disk Manager
leitura e escrita de páginas em disco
🎯 Resumo final

Até aqui foi construída a base de um sistema que:

gerencia páginas em memória
controla uso com pin_count
aplica política inteligente de cache (LRU-K)
prepara a integração com disco

👉 Isso já representa o núcleo de um banco de dados real.

---

# 📦 Slotted Pages, RID e Checksum

## 📄 Header comum (todas as páginas)

Toda página começa com 16 bytes que o buffer pool entende, independente do tipo da página (`PageHeader`, ver "Page Serialization" abaixo):

| Bytes | Campo | Uso |
|---|---|---|
| `0..8` | LSN (`u64`) | reservado para o WAL / recovery |
| `8..12` | checksum (`u32`) | CRC32 da página com este campo zerado |
| `12` | page_type (`u8`) | `0` Free (nunca formatada), `1` Slotted, `2` DatabaseHeader (só a página 0, ver "Catálogo") |
| `13` | version (`u8`) | `PAGE_FORMAT_VERSION` (hoje `1`) |
| `14..16` | reservado | zerado |

- **Escrita:** antes de gravar no disco, o buffer pool calcula o CRC32 e grava no header (`checksum::stamp_page`).
- **Leitura:** ao buscar do disco, o buffer pool recalcula o CRC32. Se não bater → `BufferPoolError::ChecksumMismatch`. Depois valida o header: tipo desconhecido ou versão diferente → `BufferPoolError::Page(..)`. Nos dois casos nenhum frame é ocupado.
- Página **toda zerada** é aceita sem checksum: é uma página nova (`PageType::Free`), que nunca foi escrita.
- `SlottedPage::new` recusa página de outro tipo formatado (`DatabaseHeader`) com `SlottedPageError::WrongPageType`; o `match` em `PageType` é exaustivo, então todo tipo novo obriga essa decisão.

## 🧱 Layout da Slotted Page (8 KB por padrão)

```
 0        8        12   13   14    16       18        20        24
 ┌────────┬────────┬────┬────┬─────┬────────┬─────────┬─────────┬─────────────────┐
 │  LSN   │checksum│type│ver │rsv  │slot_cnt│free_end │next_pg  │ slot[0] ...     │ → cresce →
 │  u64   │  u32   │ u8 │ u8 │ 2 B │  u16   │  u16    │  u32    │(off u16,len u16)│
 ├────────┴────────┴────┴────┴─────┴────────┴─────────┴─────────┴─────────────────┤
 │                     espaço livre contíguo                            │
 ├──────────────────────────────────────────────────────────────────────┤
 │              ... tupla 1 | tupla 0                                   │ ← cresce ←
 └──────────────────────────────────────────────────────────────────────┘
```

- O array de slots cresce para a direita; as tuplas crescem da direita para a esquerda.
- O fim do array de slots não é gravado: `24 + slot_count * 4`.
- `offset == 0` marca um **slot deletado** (tombstone). Tupla vazia (`len == 0`) é válida.
- Maior tupla em 8 KB: `8192 - 24 - 4 = 8164` bytes.
- Offsets são `u16`, então a página pode ter no máximo 65535 bytes.
- `next_pg` (bytes `20..24`, antes reservado) é a próxima página do Heap File; `0` = fim da lista. Páginas antigas já tinham `0` ali, então o formato não mudou de versão.

## 🔑 RID (Record ID)

`Rid { page_id: u32, slot_id: u16 }` é o endereço físico de uma tupla. Serializado em 6 bytes (little-endian), vai ser a "chave de valor" guardada nas folhas da B+Tree.

A regra mais importante do design: **o `slot_id` de uma tupla nunca muda** enquanto ela existir, então o RID continua válido mesmo quando os bytes são movidos.

## ⚙️ Operações

- **insert:** reusa o primeiro slot deletado (se houver) ou cria um novo (+4 bytes). Se o espaço contíguo não basta mas o total basta, compacta antes. Se nem o total basta → `PageFull`.
- **delete:** só marca o slot como tombstone. Os bytes viram um "buraco" (compactação lazy).
- **update:**
  - tamanho menor ou igual → sobrescreve no lugar;
  - maior → usa o espaço livre + o espaço da versão antiga, compactando se preciso, **mantendo o slot_id**;
  - não cabe → `PageFull` e a página fica intacta. Quem move a tupla para outra página é o Heap File, que devolve um `Rid` novo (ver "Heap File").
- **compact:** empurra as tuplas vivas para o fim da página e atualiza só os offsets dos slots.

## 🧠 Decisões de design

✔️ **View zero-copy:** `SlottedPage<B>` trabalha direto sobre os bytes do `Page`. `SlottedPage<&[u8]>` só lê; `SlottedPage<&mut [u8]>` também escreve.

✔️ **Compactação lazy:** delete é barato (OLTP tem muita escrita); o custo de mover bytes só aparece quando um insert/update precisa.

✔️ **Falhar sem efeito colateral:** toda operação checa o espaço antes de alterar qualquer byte.

⚠️ **Reuso de slot:** um insert reaproveita slots deletados. Quando existirem índices e MVCC, o slot só poderá ser reaproveitado depois que o índice for limpo (como o VACUUM do PostgreSQL), senão um RID antigo passaria a apontar para outra tupla.

## 🧪 Testes

- Unitários em cada módulo (`slotted_page`, `checksum`, `rid`, `page`, `buffer_pool`...).
- `random_operations_should_match_model`: 10 000 operações aleatórias comparadas com um `HashMap` e validação das invariantes a cada passo.
- Integração em `tests/storage_slotted_page.rs`: tuplas sobrevivem a eviction e a reabrir o arquivo, e corrupção no disco é detectada pelo checksum.

---

# 🧬 Page Serialization

## 📌 Visão geral

Toda conversão struct ⇄ bytes passa pelo módulo `serialization`: layout fixo, **little-endian**, sem padding e sem depender da representação em memória do Rust (nada de `transmute`/`repr(C)` sobre os bytes). Em cima dele, o `PageHeader` dá a toda página um **tipo** e uma **versão de formato**, como o `pd_pagesize_version` do PostgreSQL ou o byte de tipo das páginas de B-tree do SQLite.

## 🧰 Codec (`src/serialization.rs`)

- `ByteReader` / `ByteWriter`: cursores com `read_u8/u16/u32/u64`, `read_bytes`, `read_array::<N>`, `skip`, `write_u8/u16/u32/u64`, `write_bytes`, `write_zeros`.
- **Nunca entram em pânico:** faltou byte → `UnexpectedEof { needed, remaining }`; não cabe → `BufferOverflow { needed, remaining }`.
- **Atômicos por chamada:** um erro não avança o cursor nem escreve parte dos bytes.
- Traits `Encode` (com `ENCODED_SIZE`) e `Decode` para estruturas de tamanho fixo. Implementadas por `Rid` (6 B) e `PageHeader` (16 B). O `decode` lê o bloco inteiro de uma vez, então também é atômico.
- Helpers `get_u16/put_u16/get_u32/put_u32/get_u64/put_u64` (`pub(crate)`) para acesso por offset em código já validado (slots da slotted page, LSN, checksum), sem pagar `Result` no hot path.

## 🏷️ `PageHeader` e `PageType`

```rust
pub enum PageType { Free = 0, Slotted = 1 }

pub struct PageHeader { pub lsn: u64, pub checksum: u32, pub page_type: PageType, pub version: u8 }
```

- `PageHeader::read(page)`: decodifica e valida. Tipo desconhecido → `PageError::Serialization(InvalidValue { field: "page_type", .. })`; página formatada com versão ≠ `PAGE_FORMAT_VERSION` → `PageError::UnsupportedVersion`. Página `Free` não tem versão checada.
- `PageHeader::format(page, page_type)`: grava tipo, versão atual e zera o reservado, **mantendo LSN e checksum**.
- `SlottedPage::init` chama `format(.., Slotted)`; `SlottedPage::new` recusa página `Free` (`Uninitialized`) e header inválido (`SlottedPageError::InvalidHeader`).

## 🧠 Decisões de design

✔️ **Header comum de 16 B** (e não tipo/versão no reservado da slotted page): todo tipo de página futuro (B+Tree, FSM) herda tipo e versão de graça. A slotted page continua com `HEADER_SIZE = 24`, então a maior tupla segue 8164 B.

✔️ **Checksum continua em `8..12`:** a lógica do CRC não mudou.

✔️ **`page_type` e `version` em `u8`:** 256 valores bastam e sobram 2 bytes reservados.

✔️ **Versão global única:** se o layout de qualquer tipo mudar, sobe `PAGE_FORMAT_VERSION`.

✔️ **Validação no `fetch_page`:** página com header inválido nunca entra no pool, mesma política do checksum.

✔️ **Tipo desconhecido como `InvalidValue` do codec:** `Decode` só tem um tipo de erro (`SerializationError`), então o tipo inválido chega como `PageError::Serialization(..)`, sem um variant `UnknownPageType` duplicado.

✔️ **`match` exaustivo em `PageType`:** quando surgir um tipo novo (B+Tree), o compilador obriga `SlottedPage::new` a tratá-lo.

✔️ **Sem dependências** (`serde`/`bincode`): formato 100% controlado e documentado.

⚠️ **Formato em disco mudou:** arquivos criados com o header de 12 B não abrem mais (a página vira tipo desconhecido ou versão errada). Apague o `data.db` antigo; não há migração.

## 🧪 Testes

- Unitários em `serialization`: cada largura, limites (buffer vazio, exato, 1 byte a menos, `usize::MAX`), atomicidade dos erros e duas sequências aleatórias (1 000 casos cada) de campos tipados escritos e relidos.
- Unitários em `page`, `rid`, `slotted_page` e `buffer_pool` para header, tipos, versões e rejeição sem ocupar frame.
- Integração em `tests/storage_page_serialization.rs`: header e LSN sobrevivem a eviction e a reabrir o arquivo, tipo desconhecido/versão futura em disco são recusados, o pool continua usável e corrupção no byte de tipo é pega primeiro pelo checksum.


---

# 🧾 Tuple Storage

## 📌 Visão geral

Define como uma linha com N colunas vira os bytes guardados num slot da `SlottedPage` e como é lida de volta (`src/storage/tuple.rs`). O formato **não conhece tipos SQL**: cada campo é `&[u8]` e a coluna só diz a largura física (`Fixed(n)` ou `Variable`). A camada `types` vai converter valor ⇄ bytes em cima disso, respeitando a regra de dependência só para baixo (`storage` não importa de ninguém).

Referência: CMU 15-445 ("Database Storage II") e o `HeapTupleHeaderData` do PostgreSQL — header com NULL bitmap, colunas fixas inline e colunas variáveis apontando para uma área de dados.

## 🗂️ Organização

Desde esta atividade os módulos de storage ficam em `src/storage/` (`buffer_pool`, `checksum`, `disk_manager`, `page`, `replacer`, `rid`, `slotted_page`, `tuple`). `config` e `serialization` continuam na raiz de `src/` porque são compartilhados por todas as camadas. Testes de integração seguem `tests/<camada>_<componente>.rs`.

O `src/storage/mod.rs` reexporta os tipos principais da camada (`storage::TupleLayout`, `storage::SlottedPage`, `storage::BufferPoolManager`...).

## 🧱 Layout (little-endian, sem alinhamento)

| Offset | Tamanho | Campo |
|---|---|---|
| `0` | 2 | `column_count: u16` (tem que bater com o layout) |
| `2` | `ceil(n/8)` | NULL bitmap: bit `i % 8` do byte `i / 8` = 1 → coluna `i` é `NULL` |
| `2 + ceil(n/8)` | Σ larguras | parte fixa, na ordem das colunas: `Fixed(w)` → `w` bytes; `Variable` → `(offset u16, len u16)` |
| `data_start` | resto | bytes das colunas variáveis, na ordem das colunas |

Exemplo — layout `[Fixed(4), Variable, Fixed(1)]`, valores `(42, "ab", NULL)`:

```
00: 03 00             column_count = 3
02: 04                bitmap: coluna 2 é NULL (bit 2)
03: 2A 00 00 00       col 0 = 42
07: 0C 00 02 00       col 1 → offset 12, len 2
0B: 00                col 2 = NULL (zerado)
0C: 61 62             "ab"
```

## ⚙️ Operações

- **`TupleLayout::new(columns)`**: calcula os offsets da parte fixa uma vez por tabela. Erros: mais de 1600 colunas (`TooManyColumns`, mesmo limite do PostgreSQL), `Fixed(0)` (`ZeroWidthColumn`) e parte fixa maior que uma página (`TupleTooLarge`).
- **`layout.encode(&[Option<&[u8]>])`**: `None` = `NULL`. Valida tudo antes de alocar: número de campos (`ColumnCountMismatch`), largura das fixas (`FixedWidthMismatch`) e tamanho total ≤ 65535 (`TupleTooLarge`). Escreve em sequência com o `ByteWriter`.
- **`TupleRef::new(&layout, bytes)`**: view zero-copy. Como os bytes vêm do disco, valida **uma vez** e só aceita a forma canônica que o `encode` produz: `column_count` certo, tamanho mínimo, sem bits de padding no bitmap, coluna fixa `NULL` zerada, campos variáveis contíguos e na ordem das colunas (`NULL` variável com `len == 0`) e nenhum byte sobrando. Assim cada linha tem uma única representação em bytes. Depois disso `field(i)`, `is_null(i)` e `fields()` só falham com `ColumnOutOfRange`.

## 🧠 Decisões de design

✔️ **Formato agnóstico em `storage`**: dá para fazer antes de Data Types e mantém as camadas independentes.

✔️ **Coluna fixa `NULL` continua ocupando espaço** (o PostgreSQL omite): offsets constantes → acesso O(1) sem percorrer o bitmap.

✔️ **Slot `(offset, len)` por coluna variável**: acesso direto a qualquer coluna e validação simples; 2 B a mais que guardar só o fim.

✔️ **`column_count` gravado na tupla**: detecta leitura com o layout errado e prepara `ADD COLUMN` sem reescrever a tabela (como o `t_infomask2` do PostgreSQL).

✔️ **Sem alinhamento**: `from_le_bytes` não exige alinhamento; tuplas menores.

✔️ **`Some(&[])` ≠ `None`**: string vazia e `NULL` são diferentes, como no padrão SQL.

✔️ **`encode` devolve `Vec<u8>`**: a `SlottedPage` continua recebendo `&[u8]` e cuidando do espaço; tupla e página não se conhecem.

✔️ **Dois limites de tamanho**: o formato aceita até 65535 B (`MAX_TUPLE_SIZE`, offsets `u16`), mas a página é mais restrita — `SlottedPage::max_tuple_size` (8164 B em 8 KB) é checado no `insert`/`update`, que devolve `SlottedPageError::TupleTooLarge`.

⚠️ **Formato da tupla não tem versão própria**: é conteúdo do slot, então `PAGE_FORMAT_VERSION` não mudou. Se o layout da tupla mudar, vai precisar de versão.

## 🧪 Testes

- Unitários em `tuple`: offsets do layout (0, 1, 8, 9, 1600, 1601 colunas; `Fixed(0)`; parte fixa no limite e acima), bytes exatos do `encode`, vazio × `NULL`, cada erro e cada motivo de `Corrupted`.
- `random_tuples_should_round_trip`: 1 000 layouts e linhas aleatórias comparadas com a entrada.
- `random_corruption_should_never_panic`: 5 000 tuplas com bits trocados ou truncadas; `TupleRef::new` nunca entra em pânico e, se aceitar, todo `field` funciona (checado com teste de mutação).
- Integração em `tests/storage_tuple.rs`: tuplas numa slotted page sobrevivem a eviction e a reabrir o arquivo, update com campo maior mantém o `slot_id`, página cheia → `PageFull` com todas as linhas legíveis, tupla maior que a página → `TupleTooLarge` da slotted page, layout errado → `ColumnCountMismatch`.

## 📌 Próximos passos

- ~~Heap File~~ e ~~conversão de inteiros ⇄ campos~~: feitos (ver seções abaixo)
- Free space map: achar rápido uma página com espaço
- Schema: gerar o `TupleLayout` a partir das colunas da tabela

---

# 🗃️ Heap File

## 📌 Visão geral

O `HeapFile` (`src/storage/heap_file.rs`) guarda as tuplas de uma tabela, sem ordem, numa **lista ligada de slotted pages**. O banco inteiro é um arquivo só, então as páginas de uma tabela ficam espalhadas e cada uma aponta para a próxima pelo campo `next_page_id` do header da slotted page.

```
first_page_id
     │
     ▼
 ┌────────┐  next  ┌────────┐  next  ┌────────┐  next = 0
 │ pág. 1 │ ─────▶ │ pág. 4 │ ─────▶ │ pág. 7 │ ─────▶ fim
 └────────┘        └────────┘        └────────┘
                                     ▲ páginas novas entram aqui
```

O heap é identificado pela primeira página. Quem vai guardar o `first_page_id` de cada tabela é o catálogo (por enquanto os testes guardam na mão).

## 🧱 Alocação de páginas

- `BufferPoolManager::new_page()` devolve o próximo id livre, com a página zerada, pinada e **já dirty**: o id chega ao disco mesmo que quem pediu não escreva nada, então nunca é entregue duas vezes.
- O próximo id é `max(contador, páginas no disco, maior id no page_table + 1)`, recalculado a cada chamada: respeita páginas criadas com `fetch_page` além do fim do arquivo que ainda não foram gravadas.
- **Página 0 é reservada** (`RESERVED_PAGE_ID`) para o header do banco, que aponta para o catálogo (ver "Catálogo"). Por isso `0` pode significar "sem próxima página".
- Ids acabaram → `BufferPoolError::PageIdOverflow` (o contador é `u64` para não estourar ao entregar `u32::MAX`).

## ⚙️ Operações

- **create:** confere se o tamanho de página do pool serve para slotted pages (`UnsupportedPageSize`, sem gastar id) e aloca e formata a primeira página. Heap vazio tem sempre 1 página.
- **open:** percorre a lista no disco e guarda os ids em memória. Rejeita ciclo (`CorruptedChain`), página que não é slotted (`Page { .. }`, ex.: ponteiro para além do fim do arquivo) e começar na página 0.
- **insert:** tupla maior que uma página vazia → `TupleTooLarge` sem ler nem alocar nada. Senão pergunta ao **mapa de espaço livre** qual a primeira página (na ordem da lista) que aceita a tupla e insere nela (ver Free Space Management). Se nenhuma página conhecida cabe, segue o `next_page_id` da última: se outro handle do mesmo heap já acrescentou páginas, adota-as e tenta de novo. Só então aloca outra, grava a tupla nela e liga a anterior a ela — nunca sobrescreve um `next_page_id` existente (isso deixaria órfã a página de outro handle).
- **get:** confere que o `Rid` é de uma página deste heap (`PageNotInHeap`) e copia os bytes. Slot inexistente ou deletado → `TupleNotFound`.
- **update:** cabe na página → mesmo `Rid`. Não cabe → insere a nova versão (pode alocar página), **depois** apaga a antiga e devolve o `Rid` novo. Se o insert falhar, a versão antiga continua lá; se o delete da antiga falhar, a nova é apagada antes de devolver o erro, para não ficarem duas versões vivas.
- **delete:** tombstone no slot; o espaço volta para a página e o mapa de espaço livre registra, então o próximo insert que couber nela reaproveita o buraco.
- **scan:** `Iterator` de `(Rid, Vec<u8>)` na ordem página → slot. Cada página é pinada uma vez: as tuplas vivas são copiadas e a página é liberada antes de entregar a primeira.

## 🧠 Decisões de design

✔️ **Lista ligada no campo reservado** (como o `TableHeap` do BusTub): não muda o formato da página. Um diretório de páginas anteciparia o Free Space Management e criaria outra estrutura para manter em sincronia agora.

✔️ **`update` que não cabe muda o `Rid`** em vez de deixar um forwarding pointer. O forwarding pointer exigiria distinguir stub de tupla, evitar que o scan devolva a tupla movida duas vezes e tratar cadeias de forward. Com MVCC toda atualização vai gerar uma versão nova com endereço novo de qualquer forma; os índices atualizam o `Rid` quando ele muda. A mudança está na assinatura: `update(..) -> Result<Rid, _>`.

✔️ **`Option<NonZeroU32>` no `next_page_id`:** o tipo já diz que 0 não é uma próxima página válida.

✔️ **O `HeapFile` não guarda o buffer pool:** cada método recebe `&mut BufferPoolManager`. Várias tabelas dividem o mesmo pool sem `Rc<RefCell<_>>`.

✔️ **Pin sempre devolvido:** todo acesso passa por `read_page`/`write_page`, que fazem unpin em qualquer caminho (inclusive erro) e só marcam dirty se a escrita deu certo.

✔️ **Lista de páginas em memória:** permite validar se um `Rid` é deste heap e dispensa reler os ponteiros a cada scan. Um `HashMap` de page id → posição deixa essa checagem (e a detecção de ciclo no `open`) O(1). Custa alguns bytes por página e um `open` que lê todas as páginas.

⚠️ **Página órfã:** um erro de I/O entre alocar a página nova e gravar o ponteiro na anterior deixa uma página inalcançável (espaço perdido, nenhum dado corrompido). Resolver isso é papel do WAL.

⚠️ **`update` que move não é atômico:** o desfazer em caso de erro é de melhor esforço (se o segundo delete também falhar, as duas versões ficam). Atomicidade de verdade vem com o WAL.

⚠️ **Vários handles do mesmo heap:** o `insert` não perde dados, mas `get`/`scan` de um handle só enxergam as páginas que outro acrescentou depois do próximo `insert` dele ou de um novo `open`. O catálogo deve manter um handle por tabela.

⚠️ **`get`/`scan` copiam os bytes:** sem um guard de pin no buffer pool, devolver `&[u8]` exigiria manter a página pinada entre chamadas.

## 🧪 Testes

- Unitários em `buffer_pool` (`new_page`: página 0 pulada, ids crescentes, continua depois do disco e de páginas em memória, dirty mesmo com unpin limpo, `NoAvailableFrame` não consome id, `PageIdOverflow`) e em `slotted_page` (`next_page_id`).
- Unitários em `heap_file`: cada operação, limites (tupla vazia, máxima, máxima + 1), cada variante de erro, update que move (inclusive alocando outra página), scan com deletes e depois de mover, scan abandonado no meio, lista com ciclo / ponteiro para si mesma / para além do arquivo / página 0, insert por um handle desatualizado, tamanho de página não suportado, insert sem frame livre. Todo teste confere que nenhuma página ficou pinada.
- Falhas no meio de uma operação usam um gancho de teste do buffer pool (`fail_fetch_after`, só em `#[cfg(test)]`): `update` cujo delete falha não deixa duas versões, e o scan para depois de um erro de página.
- `random_operations_should_match_model`: 5 000 operações com pool de 3 frames contra um `HashMap<Rid, Vec<u8>>`, conferindo o scan sem duplicatas a cada 500 passos, pins e mapa de espaço livre a cada 250 e, por amostragem, que nenhuma página foi alocada enquanto outra cabia (ver Free Space Management); reabre no fim. Checado com mutação (esquecer a página nova, não marcar dirty ou ignorar o mapa faz o teste falhar).
- Integração em `tests/storage_heap_file.rs`: eviction, reabrir o arquivo, dois heaps no mesmo arquivo, páginas novas depois de reabrir não reaproveitam páginas do heap, corrupção numa página do meio detectada pelo checksum no `open`.

## 📌 Próximos passos

- Schema / Table Heap: linhas tipadas (`Value`) em cima do heap
- ~~Catálogo na página 0~~: feito (ver "Catálogo")

---

# 🧭 Free Space Management

## 📌 Visão geral

O `HeapFile` sabe quanto cabe em **cada** página dele e insere na **primeira página da lista onde a tupla cabe**. Assim reaproveita o espaço de deletes e de updates em páginas antigas, inclusive páginas que ficaram vazias; uma página nova só é alocada quando nenhuma página conhecida aceita a tupla.

O mapa de espaço livre (`FreeSpaceMap`, `src/storage/free_space_map.rs`) fica **em memória**: o `open` já lê todas as páginas para montar a lista, e o mapa sai da mesma leitura. O formato em disco não mudou.

## 🧱 O valor guardado

Para cada página, na mesma posição de `page_ids`, um `u16` que é um **limite exclusivo**: cabem tuplas de `0..valor` bytes.

| Página | `largest_insertable()` | Valor no mapa |
|---|---|---|
| vazia (8 KB) | `Some(8164)` | 8165 |
| sobram só os 4 B de um slot novo | `Some(0)` (cabe tupla vazia) | 1 |
| cheia | `None` (nem tupla vazia cabe) | 0 |

`SlottedPage::largest_insertable()` desconta os 4 B do slot novo, a não ser que exista um slot deletado para reaproveitar, e nunca passa de `max_tuple_size`. Vale `can_insert(n) ⇔ largest_insertable() >= Some(n)`.

Guardar só "a maior tupla que cabe" não funciona: `0` significaria tanto "cabe uma tupla vazia" quanto "não cabe nada", e um insert de tupla vazia encontraria a página cheia de novo para sempre. O limite exclusivo separa os dois casos.

## 🌳 Árvore de máximos

```
                 tree[1] = 80                 raiz: maior valor de todas
          ┌──────────┴──────────┐
     tree[2] = 50          tree[3] = 80
      ┌─────┴─────┐         ┌─────┴─────┐
   10 (pág 0)  50 (pág 1) 20 (pág 2)  80 (pág 3)    folhas, na ordem da lista
```

- `find(len)`: desce da raiz indo para a **esquerda** sempre que o filho esquerdo tem valor `> len` → devolve a primeira página que cabe em O(log n). Ex.: `find(30)` → página 1.
- `set(i, valor)`: muda a folha e recalcula os ancestrais, O(log n).
- `push(valor)`: página nova no fim; quando as folhas acabam, dobra a capacidade (reconstrução O(n), amortizada).
- Folhas além do número de páginas valem 0 (= cheia), então nunca são escolhidas.

## ⚙️ Como o heap usa o mapa

- **insert**: tupla > `max_tuple_size` → `TupleTooLarge` sem ler página. Senão `find(len)` → tenta a página. Se ela não aceitar (mapa desatualizado), grava o valor real (que agora não aceita `len`) e procura de novo; cada página falha no máximo uma vez, então o laço termina. Sem página no mapa → adota páginas acrescentadas por outro handle → só então aloca.
- **delete / update / insert**: a escrita devolve o novo valor da página, lido com ela ainda pinada, e o mapa só é atualizado **se a escrita deu certo**. Uma tentativa que não muda a página (insert que encontra a página cheia, update que não cabe no lugar) não a marca como dirty, então não custa uma escrita em disco.
- **open**: cada página lida para seguir a lista entra no mapa com o valor real.

## 🧠 Decisões de design

✔️ **Em memória, refeito no `open`**: montar é grátis (o `open` já lê tudo), sem mudança de formato e sem mapa desatualizado depois de um crash. Um mapa persistente só compensaria quando o `open` parar de percorrer a lista.

✔️ **Valor exato** (não categorias de 1 byte): em memória o byte a mais não pesa e, para um handle, o mapa nunca promete espaço que a página não tem.

✔️ **Primeira página que cabe** (first-fit na ordem da lista): enche o começo do heap e é o que a árvore responde barato. A ordem física dos inserts muda, mas o heap não tem ordem.

✔️ **Página vazia continua no heap**: a própria tabela reaproveita pelo mapa, sem desligar páginas da lista nem mexer na página 0.

✔️ **`FreeSpaceMap` é `pub(crate)`**: detalhe interno do heap.

⚠️ **Vários handles**: o mapa de cada handle é uma **dica**. Valor alto demais vira uma tentativa que falha e corrige o mapa; valor baixo demais só faz perder aquela página até o próximo `open`. Nenhum dado é perdido.

⚠️ **Página esvaziada não volta a aceitar a tupla máxima**: os slots deletados ficam no array (os `slot_id` são estáveis), e cada um além do reaproveitado ainda ocupa 4 B.

⚠️ **Custo por escrita**: `largest_insertable` é O(slots) (soma dos bytes vivos e procura de slot livre), a mesma ordem das operações da slotted page.

## 🧪 Testes

- Unitários em `slotted_page`: `largest_insertable` em página vazia, com tuplas, com slot reaproveitável, com buraco, só com espaço para o slot (`Some(0)`), cheia e do tamanho do header (`None`), e 5 000 estados aleatórios comparados com `can_insert`.
- Unitários em `free_space_map`: mapa vazio, primeira página que cabe, limites (`find(0)` pulando páginas cheias, `u16::MAX`), `set` para cima e para baixo, erro de índice, folhas de enchimento, crescimento por potências de 2 até 1 000 páginas e 10 000 operações contra uma busca linear.
- Unitários em `heap_file`: todo teste confere pins **e** mapa exato (`assert_heap_consistent`); reaproveitamento de buracos, first-fit, sem alocação com espaço sobrando, página esvaziada, tupla vazia em página cheia e em página com espaço só para o slot, tupla grande sem ler página, escrita que falha não muda o mapa, mapa desatualizado entre dois handles (checado com mutação: sem corrigir o mapa no erro, o insert não termina).
- `random_operations_should_match_model` do heap: confere o mapa contra as páginas a cada 250 passos e, por amostragem, que nenhuma página foi alocada enquanto outra aceitava a tupla (checado com mutação: ignorar o mapa falha no passo 0).
- Integração em `tests/storage_heap_file.rs`: espaço liberado antes de reabrir o arquivo é reaproveitado, reaproveitamento com pool de 2 frames (eviction), heap esvaziado volta a encher sem crescer.

## 📌 Próximos passos

- Schema / Table Heap: linhas tipadas (`Value`) em cima do heap
- ~~Catálogo na página 0~~: feito (ver "Catálogo"); um mapa de espaço livre persistente só se o `open` deixar de percorrer a lista
- `VACUUM`: compactar o heap e devolver páginas vazias

---

# 🔢 Data Types — Inteiros e NULL

## 📌 Visão geral

A camada `src/types/` transforma campos da tupla (`Option<&[u8]>`) em valores SQL e implementa a semântica deles: parsing de texto, casts, aritmética, comparação e `NULL`. Por enquanto só existem os inteiros do padrão SQL: `SMALLINT`, `INTEGER` e `BIGINT`. Ela depende só de `storage` (para o `ColumnLayout`), e `storage` não conhece `types`.

## 🗂️ Organização

| Arquivo | Responsabilidade |
|---|---|
| `data_type.rs` | `DataType`: nome SQL, largura, `ColumnLayout`, promoção |
| `value.rs` | `Value`: `NULL`, encode/decode, parse, `Display`, cast, operadores, comparação |
| `integer.rs` | semântica dos inteiros em `i64` + estreitamento para o tipo (privado) |
| `error.rs` | `TypeError` com mensagem e `SQLSTATE` do PostgreSQL |

## 🧱 Tipos e bytes

```rust
pub enum DataType { SmallInt, Integer, BigInt }
pub enum Value { Null, SmallInt(i16), Integer(i32), BigInt(i64) }
```

| Tipo | Rust | `ColumnLayout` | Campo na tupla | `-2` |
|---|---|---|---|---|
| `SMALLINT` | `i16` | `Fixed(2)` | complemento de 2, LE | `FE FF` |
| `INTEGER` | `i32` | `Fixed(4)` | complemento de 2, LE | `FE FF FF FF` |
| `BIGINT` | `i64` | `Fixed(8)` | complemento de 2, LE | `FE FF FF FF FF FF FF FF` |
| `NULL` | — | o da coluna | `None`: bit no NULL bitmap, parte fixa zerada | — |

`Value::decode(ty, field)` recusa campo com largura diferente de `ty.width()` (`InvalidEncoding`).

## ⚙️ Operações

- **Texto** (`Value::parse`, como `int4in`): espaços em volta (inclusive `\t\n\r\v\f`), sinal opcional e dígitos ASCII. Vazio, só sinal, `1.0`, `0x10`, `1_000`, o texto `'NULL'` → `22P02`. Não cabe no tipo → `22003`. Acumula em negativo (como `pg_strtoint64`), então `-9223372036854775808` funciona.
- **`Display`**: decimal, e `NULL` como `"NULL"`.
- **Cast**: alargar nunca falha; estreitar com range check → `OutOfRange` (`22003`). `CAST(NULL AS T)` é `NULL`.
- **Aritmética** (`add`, `sub`, `mul`, `div`, `rem`, `neg`):
  - resultado no tipo **mais largo** dos operandos; `SMALLINT + SMALLINT` é `SMALLINT` e estoura em 32767, como no PostgreSQL;
  - overflow → `OutOfRange` (`22003`), nunca wrap-around; inclui `-MIN` e `MIN / -1`;
  - `/` trunca em direção a zero (`7 / -2 = -3`); `%` tem o sinal do dividendo (`-7 % 2 = -1`);
  - `x / 0` e `x % 0` → `DivisionByZero` (`22012`); `MIN % -1 = 0`.
- **NULL** nas operações: qualquer operando `NULL` → `NULL`, antes de qualquer checagem (`NULL / 0` é `NULL`).
- **Comparação**:
  - `compare` → `Option<Ordering>`; `None` é `UNKNOWN` sempre que um lado é `NULL` (inclusive `NULL = NULL`). Compara pelo valor, independente da largura (`INTEGER 5 = BIGINT 5`).
  - `is_distinct_from` → `IS DISTINCT FROM`: `NULL` igual a `NULL`, nunca `UNKNOWN`.
  - `sort_cmp` → ordem total para `ORDER BY`/índices, com `NULL` depois de todos os valores (`NULLS LAST` do `ASC` no PostgreSQL).

## 🚨 Erros

| `TypeError` | SQLSTATE | Mensagem |
|---|---|---|
| `OutOfRange` | `22003` | `integer out of range` |
| `TextOutOfRange` | `22003` | `value "99999" is out of range for type smallint` |
| `InvalidTextRepresentation` | `22P02` | `invalid input syntax for type integer: "abc"` |
| `DivisionByZero` | `22012` | `division by zero` |
| `InvalidEncoding` | `XX001` | `integer field must be 4 bytes, got 2` |

## 🧠 Decisões de design

✔️ **Três inteiros juntos**: mesma lógica, e promoção/cast só fazem sentido com mais de uma largura.

✔️ **`Value` como enum com payload nativo** (não bytes + tipo, como o BusTub): sem alocação por valor, e o `match` exaustivo obriga cada tipo novo a ser tratado.

✔️ **Tudo em `i64`, depois estreita**: `i32 × i32` sempre cabe em `i64`, e no `BIGINT` o `checked_*` pega o overflow. Uma implementação só cobre as três larguras, sem macro nem 3×3 combinações.

✔️ **`NULL` sem tipo** (`Value::Null`, não `Null(DataType)`): o tipo de uma expressão vem do schema, decidido pelo planner; o literal `NULL` nem tem tipo até o Type System resolver pelo contexto.

✔️ **`NULL` = `None` no campo**: reusa o NULL bitmap do Tuple Storage sem mudar o formato em disco.

✔️ **`compare` e `sort_cmp` separados**: comparar precisa de `UNKNOWN`, ordenar precisa de ordem total. Misturar os dois é bug clássico.

⚠️ **`PartialEq` do Rust é estrutural**: `Null == Null` e `SmallInt(1) != BigInt(1)`. Serve para testes; filtros e joins devem usar `compare`/`is_distinct_from`.

⚠️ **Little-endian não é ordenável por bytes**: a B+Tree precisa comparar com `sort_cmp` ou usar um encoding próprio (big-endian com bit de sinal invertido).

## 🧪 Testes

- Unitários em `data_type`, `error` e `value`: limites (`MIN`, `-1`, `0`, `1`, `MAX`, `MIN-1`, `MAX+1`) em cada largura, cada erro com seu SQLSTATE e `NULL` em cada posição.
- `random_operations_should_match_the_i128_model`: 10 000 operações com valores aleatórios (viés para os extremos, ~10% `NULL`) comparadas com uma referência em `Option<i128>`; checado com mutação (remover o caso `MIN % -1` faz o teste falhar).
- `random_values_should_round_trip_through_bytes_and_text` e `sort_cmp_should_be_a_total_order` (reflexiva, antissimétrica, transitiva).
- Integração em `tests/types_integer.rs`: linhas `[SMALLINT, INTEGER, BIGINT]` com extremos e `NULL` sobrevivem a eviction e a reabrir o arquivo; linha toda `NULL` só liga o bitmap; campo lido com o tipo errado → `InvalidEncoding`.

## 📌 Próximos passos

- Boolean: `eq/lt/...` devolvendo `Value::Boolean` e a lógica de três valores de `AND`/`OR`/`NOT`
- Type System: coerção implícita × explícita e resolução de operadores entre famílias de tipos
- Float, String, Date/Time, UUID
- ~~Schema: gerar o `TupleLayout` a partir dos `DataType` das colunas~~: feito (ver "Schema")

---

# 📋 Schema

## 📌 Visão geral

A camada `src/table/` começa com o `Schema` (`schema.rs`): a lista ordenada de colunas de uma tabela e a conversão **linha de `Value` ⇄ bytes da tupla**. É a ponte entre `types` (valores) e `storage` (bytes). Dependências: `table` → `types` → `storage`, sempre para baixo.

```rust
pub struct Column { pub name: String, pub data_type: DataType, pub nullable: bool }
Column::new("age", SmallInt)        // aceita NULL
Column::not_null("id", Integer)     // NOT NULL

let schema = Schema::new(vec![Column::not_null("id", Integer), Column::new("age", SmallInt)])?;
let tuple  = schema.encode_row(&[Value::Integer(1), Value::Null])?;   // → HeapFile::insert
let row    = schema.decode_row(&tuple)?;                              // ← HeapFile::get / scan
let age    = schema.decode_column(&tuple, 1)?;                        // só uma coluna
let some   = schema.decode_columns(&tuple, &[1, 0])?;                 // várias, validando a tupla uma vez
```

O `Schema` é imutável e guarda o `TupleLayout` já calculado (`DataType::layout()` de cada coluna), então os offsets dos campos são calculados uma vez por tabela, não por linha.

## ✅ Validação do schema (`Schema::new`)

- De 1 a 1600 colunas (`TupleLayout::MAX_COLUMNS`).
- Nome não vazio, com até 63 bytes UTF-8 (`MAX_COLUMN_NAME_LEN`), e único na tabela.
- Nomes comparados **byte a byte**: `id` e `ID` são colunas diferentes. Maiúsculas/minúsculas e aspas são regra do SQL e serão resolvidas pelo parser antes de chegar aqui.

## ⚙️ Linha ⇄ tupla

- **`encode_row`** aplica a *atribuição* do SQL, coluna a coluna, e só monta a tupla se todas passarem (o primeiro erro, na ordem das colunas, é o devolvido):
  - quantidade de valores ≠ colunas → `ColumnCountMismatch`;
  - `NULL` em coluna `NOT NULL` → `NullViolation`;
  - o valor é convertido para o tipo da coluna com `Value::cast`: `SMALLINT` numa coluna `BIGINT` é aceito; `70000` numa coluna `SMALLINT` → `Type { column: "age", OutOfRange }`.
- **`decode_row` / `decode_column` / `decode_columns`** leem bytes vindos do disco e não confiam neles: tupla truncada/corrompida ou de outro schema → `Tuple(..)`; `NULL` gravado numa coluna `NOT NULL` → `NullViolation`. Os valores voltam **no tipo da coluna** (um `SmallInt` gravado numa coluna `INTEGER` volta `Integer`).
  - Todo campo é lido com `TupleRef::field`, que devolve erro: um campo que a tupla não consegue produzir nunca é confundido com `NULL`.
  - `decode_columns` devolve as colunas pedidas na ordem pedida (repetições permitidas), confere os índices **antes** de ler a tupla e valida a tupla uma vez só.
- A largura de cada campo vem do próprio schema, então `Type { .. InvalidEncoding }` não acontece na leitura; o erro de tipo só aparece no `encode_row` (valor que não cabe).

## 🚨 Erros

| `SchemaError` | SQLSTATE | Exemplo de mensagem |
|---|---|---|
| `NoColumns` | `42P16` | `a table must have at least one column` |
| `TooManyColumns` | `54011` | `tables can have at most 1600 columns, got 1601` |
| `EmptyColumnName` | `42602` | `column 2 has an empty name` |
| `ColumnNameTooLong` | `42622` | `column name "aaa…" is longer than 63 bytes` (a mensagem mostra só os primeiros 63 bytes) |
| `DuplicateColumn` | `42701` | `column "id" specified more than once` |
| `ColumnCountMismatch` | `42601` | `row has 2 values but the table has 3 columns` |
| `NullViolation` | `23502` | `null value in column "id" violates not-null constraint` |
| `ColumnOutOfRange` | `XX000` | `column 5 does not exist (table has 3)` (erro de programação) |
| `Type` | o do `TypeError` (ex.: `22003`) | `column "age": smallint out of range` |
| `Tuple` | `XX001` | `stored row does not match the schema: expected 3 columns, got 2` |

## 🧠 Decisões de design

✔️ **`NOT NULL` já no schema**, como flag da coluna checado no `encode_row`: é uma linha de código e evita mudar o formato do schema quando o catálogo existir. O item **NOT NULL** de Constraints só é marcado quando o DDL (`CREATE TABLE ... NOT NULL`) existir.

✔️ **Atribuição com cast** em vez de exigir o tipo exato: é a semântica do padrão para `INSERT`/`UPDATE` e o `Value::cast` já faz o range check.

✔️ **Linha como `Vec<Value>`**: um `struct Row` não teria invariante própria além do que o `Schema` valida.

✔️ **Limite de 63 bytes no nome**: cabe com folga num prefixo de 1 byte quando o catálogo gravar o schema. 128 caracteres (o padrão) podem passar de 255 bytes em UTF-8.

✔️ **Pelo menos uma coluna**, como no padrão. Uma tabela sem colunas complicaria catálogo e executor sem utilidade aqui.

✔️ **`decode_column` / `decode_columns`**: o `TupleRef` acessa um campo em O(1); projeção e filtro vão ler poucas colunas por linha, e `decode_columns` evita validar a mesma tupla uma vez por coluna.

✔️ **Mensagem com o nome truncado**: `ColumnNameTooLong` guarda o nome inteiro, mas o `Display` mostra só o prefixo que caberia (cortado numa fronteira de caractere) seguido de `…`, para um nome gigante não virar uma mensagem gigante.

⚠️ **Linha maior que a página**: um schema válido pode ter linhas que não cabem numa página (ex.: 1600 colunas `BIGINT` ≈ 13 KB > 8 KB). O `Schema` não conhece o tamanho de página; o `HeapFile` recusa com `TupleTooLarge` (coberto por teste de integração) e o `min_row_size()` fica disponível para o `CREATE TABLE` validar antes.

⚠️ **`NullViolation` com dois sentidos**: valor do usuário no `encode_row` ou dado inconsistente no disco no `decode_*` (mesma variante, mensagem com o nome da coluna).

## 🧪 Testes

- Unitários em `schema.rs`: `Column`, cada regra do `Schema::new` com limites (1600/1601 colunas, nome com 63/64 bytes inclusive multibyte, `id` × `ID`), cada `SchemaError` com mensagem, SQLSTATE e `source()`, `encode_row` (cast de largura, não cabe, `NULL` permitido e proibido, aridade, primeiro erro vence), `decode_row`/`decode_column`/`decode_columns` (round-trip com limites e `NULL`, tipos das colunas, ordem pedida com repetição, lista vazia, fora do intervalo antes de ler a tupla, truncada, de outro schema, `NULL` no disco em `NOT NULL`), mensagem de nome longo truncada (ASCII e multibyte).
- `random_rows_should_follow_the_assignment_rules`: 2 000 schemas e linhas aleatórios comparados com as regras calculadas à mão (sem `Value::cast`), lendo de volta com `decode_row`, `decode_column` e `decode_columns` em ordem invertida; checado com mutação (desligar o `NOT NULL` ou o cast faz o teste falhar).
- `random_corruption_should_never_panic`: 2 000 tuplas válidas com bits trocados ou truncadas.
- Integração em `tests/table_schema.rs`: 1 000 linhas tipadas num `HeapFile` sobrevivem a eviction e a reabrir o arquivo, `decode_column` projeta uma coluna das linhas guardadas, linhas recusadas pelo schema nunca chegam ao heap, linha de um schema mais largo que a página é recusada pelo heap com `TupleTooLarge`.

## 📌 Próximos passos

- ~~Table Metadata~~ e ~~validar `min_row_size()` no `CREATE TABLE`~~: feitos (ver "Catálogo")
- Table Heap: `insert(row)` / `scan()` de linhas tipadas juntando `Schema` + `HeapFile`

---

# 🗂️ Catálogo

## 📌 Visão geral

O catálogo (`src/catalog/`) é o dicionário de dados: quais tabelas existem, onde estão os dados delas e quais colunas elas têm. Ele resolve dois problemas de todo banco:

- **Bootstrap:** o catálogo também mora no disco, e alguém precisa saber onde. A **página 0** (reservada desde o Heap File) vira o **header do banco** e aponta para ele.
- **Tamanho variável:** como no PostgreSQL (`pg_class` + `pg_attribute`), tabelas e colunas ficam em **dois heaps separados**, uma entrada por tabela e uma por coluna. Uma entrada única com todas as colunas passaria de 100 KB com 1600 colunas e não caberia numa página.

```
página 0 (DatabaseHeader)
  ├── tables_first_page_id  ──▶ heap de tabelas   (TableRecord: id, first_page_id, nome)
  └── columns_first_page_id ──▶ heap de colunas   (ColumnRecord: tabela, posição, tipo, NOT NULL, nome)
                                                        │
                    TableRecord.first_page_id ──────────┴──▶ HeapFile com as linhas da tabela
```

As entradas usam **formato próprio** (`ByteWriter`/`ByteReader`): o catálogo ainda não é uma tabela comum lida com `Schema`/`Value` porque o tipo String não existe. Quando existir, o catálogo pode virar tabelas do sistema sem mudar a API.

```rust
let mut catalog = Catalog::open(&mut bpm)?;                    // cria o banco ou carrega o existente
let table = catalog.create_table(&mut bpm, "accounts", schema)?; // TableInfo { id, name, schema, first_page_id }
let table = catalog.table("accounts")?;                         // UndefinedTable se não existir
let mut heap = HeapFile::open(&mut bpm, table.first_page_id)?;  // os dados da tabela
```

## 🧱 Layout

**Página 0 (`DatabaseHeader`, `catalog/header.rs`):**

| Bytes | Campo |
|---|---|
| `0..16` | header comum, `page_type = DatabaseHeader (2)` |
| `16..24` | magic `b"ferroDB\0"` |
| `24..28` | `tables_first_page_id` (`u32` LE) |
| `28..32` | `columns_first_page_id` (`u32` LE) |
| `32..` | zerado |

O checksum pega bytes trocados; o magic recusa um arquivo cuja página 0 é válida mas não é de um banco ferroDB.

**Entradas (`catalog/records.rs`, little-endian):**

| Entrada | Campos |
|---|---|
| `TableRecord` | `table_id u32` · `first_page_id u32` · `name_len u8` · nome |
| `ColumnRecord` | `table_id u32` · `position u16` · `data_type u8` (`1` SMALLINT, `2` INTEGER, `3` BIGINT) · `flags u8` (bit 0 = nullable) · `name_len u8` · nome |

Nomes têm de 1 a 63 bytes UTF-8. A leitura recusa tipo ou flag desconhecidos (o `0` é inválido de propósito, para uma entrada zerada nunca decodificar), nome com tamanho fora do limite ou que não é UTF-8, e bytes faltando ou sobrando. Uma entrada tem no máximo 74 bytes.

## ⚙️ Operações

- **`open`:**
  - página 0 `Free` → banco novo: cria os dois heaps e grava o header **por último** (até lá, o próximo `open` começa de novo);
  - página 0 `DatabaseHeader` → confere o magic, abre os dois heaps, decodifica todas as entradas e monta cada `Schema` pela ordem de `position`;
  - página 0 de outro tipo ou magic errado → `NotADatabase`.
- **Carga e validação:** colunas numeradas `0..n` sem buraco nem repetição, `Schema::new` válido (`CorruptedSchema`), nomes e ids de tabela únicos (`Corrupted`). Colunas de um `table_id` sem `TableRecord` são **órfãs** (sobra de um `create_table` que falhou) e são ignoradas; o próximo id é `1 + maior id visto nas tabelas e nas colunas`, para um id órfão nunca ser reusado.
- **`create_table`**, nesta ordem:
  1. valida o nome (vazio, mais de 63 bytes, repetido) e se a linha cabe numa página (`schema.min_row_size() <= max_tuple_size_for(page_size)`, senão `RowTooWide`);
  2. **consome o id** da tabela;
  3. cria o `HeapFile` dos dados;
  4. grava uma `ColumnRecord` por coluna;
  5. grava a `TableRecord` **por último**: é ela que faz a tabela existir no próximo `open`;
  6. só então acrescenta a tabela ao catálogo em memória.

  Se o passo 4 ou 5 falhar, as colunas já gravadas são apagadas (melhor esforço), a tabela não aparece e o mesmo nome pode ser criado de novo, mas **com outro id**: o id consumido no passo 2 nunca volta. Se o desfazer também falhar, as colunas que sobraram ficam órfãs com esse id, e nenhuma tabela nova as herda. Por isso a numeração pode ter buracos, como as sequências do PostgreSQL.
- **`table(name)` / `tables()`:** consultas em memória, sem I/O. `tables()` está em ordem de criação. Nomes comparados byte a byte (`users` ≠ `USERS`), como as colunas.

## 🚨 Erros

| `CatalogError` | SQLSTATE | Exemplo de mensagem |
|---|---|---|
| `NotADatabase` | `XX001` | `not a ferroDB database: wrong magic number` |
| `EmptyTableName` | `42602` | `a table name cannot be empty` |
| `TableNameTooLong` | `42622` | `table name "aaa…" is longer than 63 bytes` |
| `DuplicateTable` | `42P07` | `table "users" already exists` |
| `UndefinedTable` | `42P01` | `table "users" does not exist` |
| `RowTooWide` | `54000` | `rows of table "wide" need 9000 bytes, more than the 8164 that fit a page` |
| `TooManyTables` | `54000` | `no table ids left` |
| `Corrupted` | `XX001` | `catalog is corrupted: unknown column type` |
| `CorruptedSchema` | `XX001` | `catalog is corrupted: table "t": a table must have at least one column` |
| `Heap` / `BufferPool` | `XX000` / `58030` | mensagem do erro embrulhado |

## 🧠 Decisões de design

✔️ **Formato próprio agora (opção B)**, sem esperar a String para fazer o catálogo como tabelas comuns. A API (`TableInfo`, `create_table`) não muda quando isso acontecer.

✔️ **Dois heaps**, como `pg_class`/`pg_attribute`: nenhuma entrada cresce com o número de colunas.

✔️ **`PageType::DatabaseHeader`** em vez de uma página slotted com uma tupla especial: o `match` exaustivo obriga cada leitor de página a decidir o que fazer com ela. O `PAGE_FORMAT_VERSION` não mudou, porque nenhuma página existente mudou de formato.

✔️ **`TableRecord` por último** como "ponto de commit", órfãs ignoradas e ids nunca reusados (consumidos antes de gravar na mesma sessão; `1 + maior id visto` ao carregar): sem WAL, é o que garante que uma falha **dentro do processo**, mesmo com o desfazer falhando, nunca deixa uma tabela meio criada visível nem corrompe a próxima.

✔️ **Catálogo inteiro em memória**, carregado no `open`: dezenas de bytes por coluna, e as consultas não fazem I/O.

✔️ **`TableId(u32)` newtype**: não se confunde com id de página. O contador interno é `u64`, para um id `u32::MAX` gravado ainda carregar; criar a próxima tabela dá `TooManyTables`.

✔️ **O catálogo não guarda handles `HeapFile` das tabelas**, só o `first_page_id`: o `open` não lê as páginas de dados. Os handles (um por tabela, ver Heap File) ficam para o Table Heap/executor.

⚠️ **Crash no meio de um `create_table` ou da criação do banco:** o buffer pool grava páginas em qualquer ordem, então depois de um crash real o "`TableRecord` por último" não é garantido; podem sobrar colunas órfãs (ignoradas) ou, no pior caso, uma `TableRecord` sem colunas (`CorruptedSchema` no `open`). A solução completa é o WAL.

⚠️ **Páginas órfãs:** o heap de dados de um `create_table` que falhou e os heaps de uma criação de banco interrompida ficam sem dono até um `VACUUM`.

⚠️ **Formato provisório:** quando o catálogo virar tabelas do sistema, bancos antigos não abrem (sem migração; os bancos de teste são descartáveis).

## ▶️ Demo (`cargo run`)

`src/main.rs` abre ou cria `data.db`, cria a tabela `accounts (id INTEGER NOT NULL, age SMALLINT, balance BIGINT)` na primeira execução, insere uma linha por execução e lista as tabelas e as linhas. Rodar várias vezes mostra os dados sobrevivendo. Um `data.db` da demo antiga (slotted page na página 0) dá `NotADatabase` com o aviso para apagar o arquivo.

## 🧪 Testes

- `header.rs`: bytes documentados, ida e volta, LSN preservado e reservado zerado, página nunca escrita → `None`, página slotted ou magic errado → `NotADatabase`, página pequena demais.
- `records.rs`: bytes documentados, ida e volta (nome de 1 e 63 bytes, multibyte, cada tipo, nullable ou não), cada motivo de corrupção (truncada em cada tamanho, bytes sobrando, tipo `0`/`4`, flag desconhecida, nome com 0/64 bytes, UTF-8 inválido), 2 000 entradas aleatórias de ida e volta e 2 000 corrompidas sem panic.
- `catalog.rs`: criação do banco e reabertura vazia, página 0 que não é header, `create_table` (busca, ordem e ids, nome inválido/repetido, `id` × `ID`, `RowTooWide` no limite exato, heap de dados que aceita linhas do schema, `TooManyTables`, inclusive com o id `u32::MAX` vindo do disco), carga (todas as tabelas de volta, ids continuam, 1 000 colunas em várias páginas com eviction, órfãs ignoradas sem reusar id, colunas com buraco ou repetidas, schema inválido, tabela sem colunas, nome ou id repetido, entrada corrompida), falhas injetadas (no meio do `create_table`, na entrada da tabela e entre colunas: nenhuma tabela visível, nem depois de reabrir; **desfazer que também falha**: o id não é reusado e o banco continua abrindo; gravação do header que falha: o próximo `open` cria o banco do zero) e cada erro com mensagem, SQLSTATE e `source()`.
- `random_creations_should_match_model_across_reopens`: 300 criações aleatórias (nomes repetidos e inválidos, 1 a 30 colunas) com pool de 3 frames, reabrindo a cada 50 e comparando com um `Vec`.
- Mutações conferidas: tirar o desfazer das colunas quebra os testes de falha; voltar o próximo id a 1 depois de reabrir quebra o teste de modelo.
- Integração em `tests/catalog_table_metadata.rs`: tabelas e linhas de duas tabelas sobrevivem a reabrir o arquivo (pool de 2 frames), arquivo com página 0 slotted → `NotADatabase`, página do heap de colunas corrompida no disco → checksum no `open`.

## 📌 Próximos passos

- Table Heap: `insert`/`scan` de linhas tipadas por nome de tabela, com um handle `HeapFile` por tabela
- String/VARCHAR e, com ela, o catálogo como tabelas do sistema (`INFORMATION_SCHEMA`)
- `DROP TABLE` e metadados de índice e de constraint

---

# 📏 Desvios do padrão SQL

| Assunto | Padrão (ISO/IEC 9075) | ferroDB |
|---|---|---|
| Literais inteiros | só decimais | idem; o PostgreSQL 16 aceita `1_000`, `0x1F`, `0o17`, `0b101`, o ferroDB ainda não |
| Divisão inteira | arredondamento definido pela implementação | trunca em direção a zero (PostgreSQL) |
| Tipo do resultado de `a op b` | definido pela implementação | o mais largo dos operandos (PostgreSQL) |
| Posição do `NULL` no `ORDER BY` | definida pela implementação | depois dos valores no `ASC` (PostgreSQL) |
| `MOD(a, b)` | função | por enquanto só a semântica, como operador `%`; a função entra com o parser |
| Exibição de `NULL` | não definida | `"NULL"` no `Display` (o `psql` mostra vazio) |
| Tabela sem colunas | `CREATE TABLE` exige ao menos uma coluna | idem (`NoColumns`); o PostgreSQL aceita tabela sem colunas |
| Tamanho do nome de coluna | definido pela implementação, em caracteres | até 63 **bytes** UTF-8; nome maior é recusado (`42622`), o PostgreSQL trunca em silêncio |
| Nomes de coluna iguais | comparação depois da normalização do identificador | o schema compara bytes exatos; a normalização (maiúsculas/aspas) fica para o parser |
| Esquemas (`CREATE SCHEMA`, `esquema.tabela`) | toda tabela pertence a um esquema | um único espaço de nomes para as tabelas |
| Catálogo consultável | `INFORMATION_SCHEMA.TABLES` / `.COLUMNS` | ainda não exposto via SQL; o acesso é a API `Catalog` |
| Tamanho do nome de tabela | definido pela implementação, em caracteres | até 63 **bytes** UTF-8, recusado acima disso (`42622`), como as colunas |
