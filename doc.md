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
| `12` | page_type (`u8`) | `0` Free (nunca formatada), `1` Slotted |
| `13` | version (`u8`) | `PAGE_FORMAT_VERSION` (hoje `1`) |
| `14..16` | reservado | zerado |

- **Escrita:** antes de gravar no disco, o buffer pool calcula o CRC32 e grava no header (`checksum::stamp_page`).
- **Leitura:** ao buscar do disco, o buffer pool recalcula o CRC32. Se não bater → `BufferPoolError::ChecksumMismatch`. Depois valida o header: tipo desconhecido ou versão diferente → `BufferPoolError::Page(..)`. Nos dois casos nenhum frame é ocupado.
- Página **toda zerada** é aceita sem checksum: é uma página nova (`PageType::Free`), que nunca foi escrita.

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
                                     ▲ inserts vão aqui
```

O heap é identificado pela primeira página. Quem vai guardar o `first_page_id` de cada tabela é o catálogo (por enquanto os testes guardam na mão).

## 🧱 Alocação de páginas

- `BufferPoolManager::new_page()` devolve o próximo id livre, com a página zerada, pinada e **já dirty**: o id chega ao disco mesmo que quem pediu não escreva nada, então nunca é entregue duas vezes.
- O próximo id é `max(contador, páginas no disco, maior id no page_table + 1)`, recalculado a cada chamada: respeita páginas criadas com `fetch_page` além do fim do arquivo que ainda não foram gravadas.
- **Página 0 é reservada** (`RESERVED_PAGE_ID`) para o header do banco / raiz do catálogo. Por isso `0` pode significar "sem próxima página".
- Ids acabaram → `BufferPoolError::PageIdOverflow` (o contador é `u64` para não estourar ao entregar `u32::MAX`).

## ⚙️ Operações

- **create:** confere se o tamanho de página do pool serve para slotted pages (`UnsupportedPageSize`, sem gastar id) e aloca e formata a primeira página. Heap vazio tem sempre 1 página.
- **open:** percorre a lista no disco e guarda os ids em memória. Rejeita ciclo (`CorruptedChain`), página que não é slotted (`Page { .. }`, ex.: ponteiro para além do fim do arquivo) e começar na página 0.
- **insert:** tenta só a **última página**. Se está cheia, primeiro segue o `next_page_id` dela: se outro handle do mesmo heap já acrescentou páginas, adota-as e tenta de novo na última de verdade. Só então aloca outra, grava a tupla nela e liga a anterior a ela — nunca sobrescreve um `next_page_id` existente (isso deixaria órfã a página de outro handle). Tupla que não caberia nem numa página vazia → `TupleTooLarge` sem alocar nada.
- **get:** confere que o `Rid` é de uma página deste heap (`PageNotInHeap`) e copia os bytes. Slot inexistente ou deletado → `TupleNotFound`.
- **update:** cabe na página → mesmo `Rid`. Não cabe → insere a nova versão (pode alocar página), **depois** apaga a antiga e devolve o `Rid` novo. Se o insert falhar, a versão antiga continua lá; se o delete da antiga falhar, a nova é apagada antes de devolver o erro, para não ficarem duas versões vivas.
- **delete:** tombstone no slot; o espaço volta para a página, mas só é reaproveitado por inserts quando ela é a última (ver Free Space Management).
- **scan:** `Iterator` de `(Rid, Vec<u8>)` na ordem página → slot. Cada página é pinada uma vez: as tuplas vivas são copiadas e a página é liberada antes de entregar a primeira.

## 🧠 Decisões de design

✔️ **Lista ligada no campo reservado** (como o `TableHeap` do BusTub): não muda o formato da página. Um diretório de páginas anteciparia o Free Space Management e criaria outra estrutura para manter em sincronia agora.

✔️ **`update` que não cabe muda o `Rid`** em vez de deixar um forwarding pointer. O forwarding pointer exigiria distinguir stub de tupla, evitar que o scan devolva a tupla movida duas vezes e tratar cadeias de forward. Com MVCC toda atualização vai gerar uma versão nova com endereço novo de qualquer forma; os índices atualizam o `Rid` quando ele muda. A mudança está na assinatura: `update(..) -> Result<Rid, _>`.

✔️ **`Option<NonZeroU32>` no `next_page_id`:** o tipo já diz que 0 não é uma próxima página válida.

✔️ **O `HeapFile` não guarda o buffer pool:** cada método recebe `&mut BufferPoolManager`. Várias tabelas dividem o mesmo pool sem `Rc<RefCell<_>>`.

✔️ **Pin sempre devolvido:** todo acesso passa por `read_page`/`write_page`, que fazem unpin em qualquer caminho (inclusive erro) e só marcam dirty se a escrita deu certo.

✔️ **Lista de páginas em memória:** permite validar se um `Rid` é deste heap e dispensa reler os ponteiros a cada scan. Custa 4 B por página e um `open` que lê todas as páginas.

⚠️ **Inserts só na última página:** espaço liberado em páginas antigas fica ocioso até o Free Space Management.

⚠️ **Página órfã:** um erro de I/O entre alocar a página nova e gravar o ponteiro na anterior deixa uma página inalcançável (espaço perdido, nenhum dado corrompido). Resolver isso é papel do WAL.

⚠️ **`update` que move não é atômico:** o desfazer em caso de erro é de melhor esforço (se o segundo delete também falhar, as duas versões ficam). Atomicidade de verdade vem com o WAL.

⚠️ **Vários handles do mesmo heap:** o `insert` não perde dados, mas `get`/`scan` de um handle só enxergam as páginas que outro acrescentou depois do próximo `insert` dele ou de um novo `open`. O catálogo deve manter um handle por tabela.

⚠️ **`get`/`scan` copiam os bytes:** sem um guard de pin no buffer pool, devolver `&[u8]` exigiria manter a página pinada entre chamadas.

## 🧪 Testes

- Unitários em `buffer_pool` (`new_page`: página 0 pulada, ids crescentes, continua depois do disco e de páginas em memória, dirty mesmo com unpin limpo, `NoAvailableFrame` não consome id, `PageIdOverflow`) e em `slotted_page` (`next_page_id`).
- Unitários em `heap_file`: cada operação, limites (tupla vazia, máxima, máxima + 1), cada variante de erro, update que move (inclusive alocando outra página), scan com deletes e depois de mover, scan abandonado no meio, lista com ciclo / ponteiro para si mesma / para além do arquivo / página 0, insert por um handle desatualizado, tamanho de página não suportado, insert sem frame livre. Todo teste confere que nenhuma página ficou pinada.
- Falhas no meio de uma operação usam um gancho de teste do buffer pool (`fail_fetch_after`, só em `#[cfg(test)]`): `update` cujo delete falha não deixa duas versões, e o scan para depois de um erro de página.
- `random_operations_should_match_model`: 5 000 operações com pool de 3 frames contra um `HashMap<Rid, Vec<u8>>`, conferindo o scan sem duplicatas a cada 500 passos e reabrindo no fim; checado com mutação (esquecer a página nova, ou não marcar dirty, faz o teste falhar).
- Integração em `tests/storage_heap_file.rs`: eviction, reabrir o arquivo, dois heaps no mesmo arquivo, páginas novas depois de reabrir não reaproveitam páginas do heap, corrupção numa página do meio detectada pelo checksum no `open`.

## 📌 Próximos passos

- Free Space Management: achar uma página com espaço sem percorrer a lista e reaproveitar o espaço de deletes
- Schema / Table Heap: linhas tipadas (`Value`) em cima do heap
- Catálogo na página 0: guardar o `first_page_id` de cada tabela

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
- Schema: gerar o `TupleLayout` a partir dos `DataType` das colunas

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
