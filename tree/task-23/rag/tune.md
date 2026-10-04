# Настройка второго этапа (задача 23)

Индекс `rag/index.sqlite` (`fixed`, 1368 чанков, 10 документов), вопросы — `docs/control.json`: 21 с ответом, 5 без. Реранкер и rewrite — `glm-5.3-flash`, temperature 0; реранкер оценил top-20 кандидатов каждого вопроса (78 этапов LLM, ≈322437 tok — повторные оценки берутся из кэша, но засчитываются по доле). Ответы здесь не генерируются — только то, что попало бы в контекст.

## 1. Сколько кандидатов брать (top-K до)

Recall@N — у скольких вопросов с ответом нужный чанк (файл + страница) есть среди первых N по косинусу.

| N | 1 | 4 | 10 | 20 | 30 | 50 |
|---|---|---|---|---|---|---|
| исходный вопрос | 15/21 | 19/21 | 20/21 | 20/21 | 20/21 | 21/21 |
| вопрос + rewrite | 17/21 | 19/21 | 20/21 | 20/21 | 20/21 | 20/21 |

## 2. Порог similarity (z-скор косинуса)

top-20 кандидатов → z ≥ порога → top-4. Исходный вопрос, без реранкера.

| настройка | нужный чанк в контексте | чанков Σ | из чужих документов Σ | без ответа: пустой контекст | с ответом: контекст опустел |
|---|---|---|---|---|---|
| без порога (base) | 19/21 | 104 | 23 | 0/5 | 0/21 |
| z ≥ 1.0 | 19/21 | 104 | 23 | 0/5 | 0/21 |
| z ≥ 1.5 | 19/21 | 104 | 23 | 0/5 | 0/21 |
| z ≥ 2.0 | 19/21 | 102 | 21 | 0/5 | 0/21 |
| z ≥ 2.5 | 19/21 | 83 | 8 | 2/5 | 1/21 |
| z ≥ 3.0 | 17/21 | 68 | 1 | 4/5 | 3/21 |
| z ≥ 3.5 | 13/21 | 38 | 0 | 5/5 | 6/21 |
| z ≥ 4.0 | 5/21 | 11 | 0 | 5/5 | 16/21 |
| z ≥ 4.5 | 3/21 | 3 | 0 | 5/5 | 18/21 |

## 3. Порог реранкера (LLM, 0–10)

top-20 кандидатов → LLM-оценка ≥ порога → сортировка по оценке → top-4. Исходный вопрос.

| настройка | нужный чанк в контексте | чанков Σ | из чужих документов Σ | без ответа: пустой контекст | с ответом: контекст опустел |
|---|---|---|---|---|---|
| llm ≥ 0 | 20/21 | 104 | 20 | 0/5 | 0/21 |
| llm ≥ 1 | 20/21 | 86 | 4 | 4/5 | 0/21 |
| llm ≥ 2 | 20/21 | 85 | 4 | 4/5 | 0/21 |
| llm ≥ 3 | 20/21 | 76 | 4 | 4/5 | 0/21 |
| llm ≥ 4 | 20/21 | 74 | 4 | 4/5 | 1/21 |
| llm ≥ 5 | 19/21 | 58 | 0 | 5/5 | 2/21 |
| llm ≥ 6 | 19/21 | 56 | 0 | 5/5 | 2/21 |
| llm ≥ 7 | 19/21 | 56 | 0 | 5/5 | 2/21 |
| llm ≥ 8 | 17/21 | 30 | 0 | 5/5 | 4/21 |
| llm ≥ 9 | 16/21 | 23 | 0 | 5/5 | 5/21 |

## 4. top-K до для реранкера

Порог llm ≥ 5, после — top-4.

| настройка | нужный чанк в контексте | чанков Σ | из чужих документов Σ | без ответа: пустой контекст | с ответом: контекст опустел |
|---|---|---|---|---|---|
| top-4 → top-4 | 18/21 | 39 | 0 | 5/5 | 3/21 |
| top-10 → top-4 | 19/21 | 53 | 0 | 5/5 | 2/21 |
| top-20 → top-4 | 19/21 | 58 | 0 | 5/5 | 2/21 |

## 5. Вместе с rewrite

Кандидаты — лучший z из исходного вопроса и переписанного; top-20 → фильтры → top-4.

| настройка | нужный чанк в контексте | чанков Σ | из чужих документов Σ | без ответа: пустой контекст | с ответом: контекст опустел |
|---|---|---|---|---|---|
| rewrite, без фильтра | 19/21 | 104 | 23 | 0/5 | 0/21 |
| rewrite + z ≥ 2.0 | 19/21 | 104 | 23 | 0/5 | 0/21 |
| rewrite + z ≥ 2.5 | 19/21 | 85 | 9 | 2/5 | 1/21 |
| rewrite + z ≥ 3.0 | 17/21 | 75 | 6 | 3/5 | 3/21 |
| rewrite + llm ≥ 3 | 20/21 | 82 | 5 | 4/5 | 0/21 |
| rewrite + z ≥ 2.0 + llm ≥ 3 | 20/21 | 81 | 5 | 4/5 | 1/21 |
| rewrite + z ≥ 2.5 + llm ≥ 3 | 20/21 | 73 | 4 | 4/5 | 1/21 |
| rewrite + llm ≥ 5 | 19/21 | 62 | 0 | 5/5 | 2/21 |
| rewrite + z ≥ 2.0 + llm ≥ 5 | 19/21 | 62 | 0 | 5/5 | 2/21 |
| rewrite + z ≥ 2.5 + llm ≥ 5 | 19/21 | 61 | 0 | 5/5 | 2/21 |
| rewrite + llm ≥ 7 | 19/21 | 59 | 0 | 5/5 | 2/21 |
| rewrite + z ≥ 2.0 + llm ≥ 7 | 19/21 | 59 | 0 | 5/5 | 2/21 |
| rewrite + z ≥ 2.5 + llm ≥ 7 | 19/21 | 58 | 0 | 5/5 | 2/21 |

## 6. По вопросам

Ранг — позиция первого нужного чанка среди 50 кандидатов. z — z-скор top-1 и нужного чанка; llm — оценка нужного чанка реранкером (у вопросов без ответа — лучшая оценка среди кандидатов).

| # | вопрос | ранг | ранг с rewrite | z top-1 | z нужного | llm нужного / max | запрос после rewrite |
|---|---|---|---|---|---|---|---|
| 1 | What randomized election timeout range does Raft recommend, and why are the timeouts randomized? | 1 | 1 | 3.74 | 3.74 | 10 / 10 | What range of randomized election timeouts does the Raft consensus algorithm recommend, and what is the purpose of randomizing election timeouts to split votes and prevent split elections? |
| 2 | raft cluster membership change? | 2 | 1 | 3.71 | 3.61 | 9 / 9 | How does the Raft consensus algorithm handle changes to cluster membership, such as adding or removing servers, and what mechanisms like joint consensus or single-server changes ensure safety during configuration changes? |
| 3 | What were the masses of the two black holes in GW150914, the mass of the final black hole, and how much mass was radiated as gravitational waves? | 1 | 1 | 4.87 | 4.87 | 10 / 10 | What were the source masses of the two black holes in the GW150914 LIGO gravitational-wave detection, the mass of the final black hole after merger, and how much mass was radiated away as gravitational waves? Key terms: binary black hole merger, observed source masses, final mass, energy radiated as gravitational waves, general relativity, chirp signal. |
| 4 | How long are the arms of the Advanced LIGO detectors and how much laser power circulates in each arm cavity? | 1 | 1 | 4.50 | 4.50 | 10 / 10 | What is the arm length and circulating laser power of the Advanced LIGO gravitational wave detectors, and how do these relate to strain sensitivity? Key terms: LIGO Hanford Livingston, Fabry-Perot arm cavity, kilometer-scale interferometer, laser interferometry, gravitational wave detection, photon shot noise, detector sensitivity. |
| 5 | How many positions per second does AlphaZero search in chess and shogi, compared with Stockfish and Elmo? | 1 | 1 | 4.69 | 4.69 | 10 / 10 | How many positions per second does the AlphaZero system evaluate during its self-play search in chess and shogi, compared with the search speeds of the traditional engines Stockfish and Elmo? Key terms: Monte Carlo tree search, evaluation, nodes per second, search speed, self-play, superhuman performance. |
| 6 | alphazero training hardware? | 1 | 1 | 3.71 | 3.71 | 10 / 10 | What hardware and computing resources were used to train AlphaZero for chess and shogi, including TPUs, self-play games, and reinforcement learning training infrastructure? |
| 7 | What does NISQ stand for, and roughly how many gates can such a device execute before noise overwhelms the signal? | 2 | 1 | 3.60 | 3.28 | 10 / 10 | What does NISQ (Noisy Intermediate-Scale Quantum) stand for, and roughly how many quantum gates can a noisy intermediate-scale quantum device execute before noise overwhelms the computation? Key terms: quantum computing, qubits, gate fidelity, decoherence, error rates, depth of quantum circuits, John Preskill. |
| 8 | How do the Spectre authors propose to mitigate the conditional-branch variant, and why is indirect branch poisoning harder to mitigate? | 8 | 9 | 3.59 | 2.73 | 4 / 4 | How do the Spectre paper authors propose to mitigate Spectre variant 1 (bounds check bypass via conditional branch misprediction), and why is Spectre variant 2 (branch target injection via indirect branch poisoning) harder to mitigate? — mitigation techniques, speculative execution, branch predictor poisoning, serializing instructions, microarchitectural side channel |
| 9 | For what kind of optimization problems is Bayesian optimization best suited, according to the tutorial? | 1 | 1 | 3.92 | 3.92 | 10 / 10 | According to Peter Frazier's "A Tutorial on Bayesian Optimization," for which kinds of optimization problems is Bayesian optimization best suited, such as expensive black-box functions and costly function evaluations? key terms: derivative-free, sample efficiency, hyperparameter tuning, expensive-to-evaluate objective, Gaussian process surrogate, acquisition function |
| 10 | Which acquisition functions does the Bayesian optimization tutorial describe? | 1 | 2 | 3.88 | 3.88 | 10 / 10 | What acquisition functions are described in Peter Frazier's tutorial on Bayesian optimization, and how do they guide the selection of the next evaluation point? Expected key terms: expected improvement, probability of improvement, upper confidence bound, knowledge gradient, surrogate model, Gaussian process, exploration-exploitation trade-off. |
| 11 | What share of the US workforce could have at least 10% of their work tasks affected by LLMs, and what share at least 50%? | 1 | 1 | 3.77 | 3.77 | 10 / 10 | What percentage of US labor market employment could have at least 10% and at least 50% of work tasks affected by large language models, according to GPTs are GPTs labor market impact study? key terms: exposure, occupations, workforce, GPT, tasks affected, wage distribution, economic impact |
| 12 | In the GPTs-are-GPTs labor study, who applied the exposure rubric and to which occupational dataset? | 1 | 1 | 3.78 | 3.78 | 8 / 8 | In the GPTs are GPTs paper on large language models' impact on the labor market, which human annotators or organization applied the exposure rubric to the O*NET occupational database, and what were the exposure criteria such as GPT, LLM, occupation, task, human ratings? |
| 13 | What do the flags High Leverage, Long-term and Uncertain Impact mean in the paper on tackling climate change with machine learning? | 1 | 1 | 2.79 | 2.79 | 10 / 10 | What do the High Leverage, Long-term and Uncertain Impact flags mean in the paper "Tackling Climate Change with Machine Learning"? climate change machine learning applications, high leverage opportunity, long-term impact, uncertain impact, mitigation and adaptation, CO2 emissions reduction, prioritization criteria |
| 14 | What share of global greenhouse gas emissions comes from cement and steel production? | 1 | 1 | 3.00 | 3.00 | 7 / 7 | What percentage of global greenhouse gas emissions is attributable to cement and steel manufacturing, and what role can machine learning play in climate change mitigation? emissions share, industrial processes, decarbonization, climate mitigation, machine learning applications |
| 15 | Which language models and which synthetic task were used in the study showing that models get lost in the middle of long contexts? | 2 | 1 | 3.36 | 3.15 | 7 / 7 | Which language models and which synthetic retrieval task were evaluated in the "Lost in the Middle" paper on how language models use long contexts, and what does the study conclude about position of relevant information in the input context window? key terms: lost in the middle, long-context, multi-document question answering, needle-in-a-haystack, GPT-3.5, Claude, positional bias, context length |
| 16 | Small2Big? | 44 | — | 2.37 | 1.56 | — / 3 | What is Small2Big and in which context or system is this term used, e.g., scale transition, growth process, or algorithm name, with related terms such as multi-scale modeling, coarse-to-fine, hierarchy, scalability, and terminology definition? |
| 17 | According to the RAG survey, how does adding irrelevant documents to the context affect the accuracy of RAG? | 1 | 1 | 4.32 | 4.32 | 10 / 10 | According to the Retrieval-Augmented Generation for Large Language Models survey, how does adding irrelevant documents to the context affect RAG accuracy? noise distractors retrieval-augmented generation context length answer degradation |
| 18 | In the paper on tackling climate change with machine learning, which renewable-energy hardware has been controlled with reinforcement learning or Bayesian optimization to maximize electricity production? | 1 | 1 | 2.56 | 2.56 | 10 / 10 | In "Tackling Climate Change with Machine Learning," which renewable energy hardware systems—such as wind turbines or solar farms—have been controlled using reinforcement learning or Bayesian optimization to maximize electricity production? Key terms: wind farm control, turbine yaw optimization, solar panel, power output, energy generation, learning-based control. |
| 19 | What is single-decree Paxos, and which problem with Paxos motivated the design of Raft? | 1 | 1 | 3.61 | 3.61 | 9 / 9 | What is single-decree Paxos, and what problems with Paxos as a consensus algorithm motivated the design of the Raft consensus algorithm? Paxos, single-decree consensus, hard to understand, Multi-Paxos, leader election, log replication, understandability |
| 20 | Can a Spectre attack be mounted from a web page? | 4 | 3 | 3.17 | 3.04 | 10 / 10 | Can a Spectre side-channel attack, which exploits speculative execution in modern CPUs, be launched from JavaScript running in a web browser on a malicious web page? Spectre, Meltdown, CPU cache side channel, branch prediction, JavaScript, browser sandbox, timing attack, cache eviction |
| 21 | When the relevant document sits in the middle of a 20-document context, how does GPT-3.5-Turbo compare with answering closed-book, without any documents? | 1 | 1 | 4.31 | 4.31 | 10 / 10 | When relevant documents are positioned in the middle of a long context window of 20 distractor documents, how does GPT-3.5-Turbo's open-book answering performance compare with closed-book performance where no documents are provided in the context at all? Key terms: Lost in the Middle, long contexts, position of relevant information, retrieval accuracy, degradation, performance difference, closed-book baseline, language models use long contexts. |
| 22 | What is the recommended ibuprofen dose for a five-year-old child? | — | — | 2.58 | — | — / 0 | What is the recommended ibuprofen dose (pediatric dosing, milligrams per kilogram, weight-based dosing) for a five-year-old child according to medical dosing guidelines? |
| 23 | What block time does Bitcoin target, and how is its mining difficulty adjusted? | — | — | 2.30 | — | — / 0 | What block time interval does the Bitcoin protocol target on average, and how and when is its mining difficulty adjusted to keep that interval stable? Keywords: proof of work, block interval, difficulty retargeting, target, hash rate, Nakamoto consensus. |
| 24 | Which national team won the 2018 FIFA World Cup? | — | — | 2.31 | — | — / 0 | Which national soccer team won the 2018 FIFA World Cup tournament in Russia, and who did they defeat in the final match? football championship winner final result |
| 25 | How many qubits did Google's Sycamore processor use in its 2019 quantum supremacy experiment? | — | — | 2.66 | — | — / 4 | How many qubits did Google's Sycamore quantum processor use in its 2019 quantum supremacy experiment demonstrating sampling beyond classical supercomputer capability, in terms of quantum processor qubit count and random circuit sampling? |
| 26 | What was the signal-to-noise ratio of the binary neutron star merger GW170817? | — | — | 3.03 | — | — / 0 | What was the signal-to-noise ratio at which the binary neutron star merger event GW170817 was detected by LIGO and Virgo gravitational wave detectors, as reported in the observation paper? Include terms like GW170817, signal-to-noise ratio, detection significance, gravitational-wave signal, merger event. |
