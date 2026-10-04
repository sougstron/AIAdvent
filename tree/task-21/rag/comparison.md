# Сравнение стратегий чанкинга

| метрика | fixed | structure |
|---|---|---|
| параметры | `{"overlap":200,"size":1000}` | `{"struct_max":4000}` |
| модель / dim | nomic-embed-text / 768 | nomic-embed-text / 768 |
| чанков | 137 | 49 |
| символов min / медиана / среднее / max | 649 / 997 / 993 / 1000 | 18 / 2218 / 2233 / 3980 |
| мелких (< 200 симв.) | 0 (0%) | 1 (2%) |
| избыточность (Σ чанков / текст) | 1.24× | 1.00× |
| пересекают границу раздела | 37 (27%) | 0 (0%) |
| обрываются посреди предложения | 116 (85%) | 5 (10%) |
| время эмбеддингов | 20927 мс (153 мс/чанк) | 18315 мс (374 мс/чанк) |
| поиск: hit@1 (чанк задевает раздел) | 10/12 | 9/12 |
| поиск: hit@1 (чанк целиком в разделе) | 2/12 | 9/12 |
| поиск: hit@3 | 11/12 | 11/12 |
| поиск: MRR | 0.88 | 0.85 |

## Пробные вопросы (docs/questions.json)

Ранг — позиция первого чанка из ожидаемого раздела (0 — нет в выдаче).

| вопрос | ожидаемый раздел | fixed: ранг / cos / top-1 | structure: ранг / cos / top-1 |
|---|---|---|---|
| What are the three paradigms of retrieval-augmented generation? | II. OVERVIEW OF RAG | 12 / 0.803 / REFERENCES | 7 / 0.794 / Front matter |
| What are the drawbacks of Naive RAG? | Naive RAG | 1 / 0.757 / II. OVERVIEW OF RAG > A. Naive RAG | 2 / 0.699 / VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness |
| When should I use RAG instead of fine-tuning the model? | RAG vs Fine-tuning | 1 / 0.807 / II. OVERVIEW OF RAG > D. RAG vs Fine-tuning | 1 / 0.835 / II. OVERVIEW OF RAG > D. RAG vs Fine-tuning |
| How can the user query be rewritten or expanded before retrieval? | Query Optimization | 2 / 0.750 / II. OVERVIEW OF RAG > B. Advanced RAG … II. OVERVIEW OF RAG > C. Modular RAG | 1 / 0.750 / III. RETRIEVAL > C. Query Optimization (part 1/2) |
| How are chunk size and metadata used to optimize the index? | Indexing Optimization | 1 / 0.779 / III. RETRIEVAL > A. Retrieval Source … III. RETRIEVAL > B. Indexing Optimization | 1 / 0.753 / III. RETRIEVAL > B. Indexing Optimization |
| How are embedding models fine-tuned for retrieval? | D. Embedding | 1 / 0.810 / III. RETRIEVAL > C. Query Optimization … III. RETRIEVAL > D. Embedding | 1 / 0.811 / III. RETRIEVAL > D. Embedding |
| How does iterative retrieval alternate between retrieving and generating? | Iterative Retrieval | 1 / 0.817 / V. AUGMENTATION PROCESS IN RAG … V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval | 1 / 0.825 / V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval |
| How does the model decide by itself when to retrieve? | Adaptive Retrieval | 1 / 0.725 / V. AUGMENTATION PROCESS IN RAG > C. Adaptive Retrieval … VI. TASK AND EVALUATION | 2 / 0.680 / IV. GENERATION |
| Which benchmarks and tools are used to evaluate RAG systems? | Evaluation | 1 / 0.876 / VI. TASK AND EVALUATION > C. Evaluation Aspects … VII. DISCUSSION AND FUTURE PROSPECTS | 1 / 0.878 / VI. TASK AND EVALUATION > D. Evaluation Benchmarks and Tools |
| Do long-context LLMs make RAG unnecessary? | RAG vs Long Context | 1 / 0.828 / VII. DISCUSSION AND FUTURE PROSPECTS … VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context | 1 / 0.846 / VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context |
| How robust is RAG to noisy or irrelevant retrieved documents? | RAG Robustness | 1 / 0.818 / VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context … VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness | 1 / 0.845 / VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness |
| Can RAG work with images, audio and video? | Multi-modal RAG | 1 / 0.797 / VII. DISCUSSION AND FUTURE PROSPECTS > E. Production-Ready RAG … VII. DISCUSSION AND FUTURE PROSPECTS > F. Multi-modal RAG | 1 / 0.764 / VII. DISCUSSION AND FUTURE PROSPECTS > F. Multi-modal RAG |
