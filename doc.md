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