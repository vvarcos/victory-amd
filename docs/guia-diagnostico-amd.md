# Guia de diagnóstico AMD: problemas encontrados e soluções

Este guia reúne descobertas de testes deste fork em Windows 10 (build 19045), Ryzen 5 5600G, Radeon RX 6600M de 8 GB e 16 GB de RAM. Os resultados são específicos desse ambiente e do binário/driver testados: **não constituem garantia de compatibilidade, desempenho ou estabilidade**.

Use somente arquivos de jogo obtidos legalmente. Nenhum arquivo do jogo, save, shader extraído, dump ou cache binário é distribuído neste documento. Os comandos usam caminhos ilustrativos; adapte-os antes de executar.

**Estados usados:** solução verificada; contorno observado; investigação pendente. Uma proposta não implementada é identificada explicitamente.

## 1. Root signature recusada com flags `0xc00` / runtime D3D12 inadequado

**Problema:** o runtime D3D12 do sistema pode recusar recursos exigidos pelo título. Ter uma GPU compatível não garante que o processo esteja usando o runtime esperado.

**Contorno observado:** nos testes com Agility SDK app-local, não foram observadas novas recusas dessas flags. O binário testado exportava `D3D12SDKVersion=620` e `D3D12SDKPath=.\\D3D12\\`; o módulo carregado era `D3D12Core.dll` versão `1.620.2.0.20261106.5`.

**Como verificar:** conferir exports do executável, implantação das DLLs oficiais do SDK e caminho, versão e SHA-256 do módulo **efetivamente carregado pelo processo**. A presença da DLL ao lado do programa não basta. A pasta `D3D12` é relativa ao executável, não ao diretório de trabalho.

**Limitação importante:** a feature `agility-sdk` e seus exports foram testados em alterações locais. Este commit de documentação não publica nem garante a presença dessas alterações de código. Antes de usar `--features agility-sdk`, confirme que a sua revisão contém a feature e a configuração de exports. Não baixe DLLs de sites não oficiais.

## 2. Logs e entradas de diagnóstico não aparecem

**Problema:** opções como `VICTORY_KEYS` dependem da feature `diagnostics`; um build comum não necessariamente contém a instrumentação esperada.

**Solução:** conferir as features do build e o código da opção antes de usá-la. Registrar SHA-256 do executável e distinguir builds de diagnóstico de builds destinados à prova final. Não inferir as features pelo nome do arquivo (`victory-diag.exe` é apenas um nome local).

Também limpe variáveis `VICTORY_*` herdadas antes de preparar um experimento, para que opções de testes antigos não contaminem o próximo.

## 3. Jogo sem GPU host ou com device removed

**Problema:** um processo pode permanecer aberto mesmo sem executar comandos na GPU host. `device removed` também não equivale automaticamente a defeito físico da placa.

**Diagnóstico:** procurar `no host D3D12 device`, `device removed`, erros de criação da biblioteca e erros do driver. Conferir adapter selecionado, módulo D3D12 carregado e baseline/delta de crashes.

**Estado:** os runs recentes carregaram a GPU host e não registraram device removed; isso não resolve todas as causas possíveis. Não declarar sucesso apenas porque a janela existe.

## 4. Quadros lentos apesar de a GPU ficar ociosa

**Problema:** criação de PSOs, tradução, leituras, cópias e sincronização podem deixar a GPU esperando. Aumentar VRAM disponível ou diminuir a resolução não resolve necessariamente esse gargalo.

**Contorno verificado:** preservar e aquecer o cache da mesma rota reduz criações de pipelines. Examinar no framelog `pipelines: ... made, ... loaded, ... ms waited`, atividade da GPU, disco e cópias.

**Cuidado com métricas:** `PipelineWaitUs` soma esperas entre threads; não é tempo de parede. Uma redução de esperas não prova redução equivalente do tempo total do frame. Não atribuir todos os stalls a pipelines sem analisar o quadro correspondente.

## 5. `translate --pipelines` não pré-aquece a biblioteca do jogo

**Problema:** confundir validação offline com cache reutilizável. A flag deste checkout é `--pipelines`, **não `--host`**.

**Descoberta verificada:** esse caminho cria PSOs com estado sintético e shaders auxiliares, depois as descarta. Não grava a biblioteca de PSOs usada pelo jogo. Em um teste, também terminou com access violation `0xC0000005`; não é um procedimento de pré-aquecimento recomendado.

**Solução:** usar o subcomando `warm` para tradução em cache e pipelines compute, conforme a próxima seção. `translate` sem essa opção continua útil para diagnóstico, mas contagens de shaders traduzidos não significam PSOs persistidas.

## 6. Como pré-compilar shaders e pipelines compute antes do jogo

**Solução verificada:** `warm` grava shaders traduzidos e cria pipelines **compute** reutilizáveis na biblioteca. Confira primeiro `warm --help` no seu binário.

Exemplo PowerShell, com jogo e outros processos de aquecimento parados:

```powershell
$exe = 'C:\VIctory\victory.exe'
$env:VICTORY_CACHE_DIR = 'D:\VIctory-tests\warm-cache'
$env:VICTORY_LOG = 'info'
& $exe warm --jobs 12 'D:\MeuJogo\shaders.rpf'
# Repetir com o mesmo binário, adapter, driver e cache para verificar reutilização:
& $exe warm --jobs 12 'D:\MeuJogo\shaders.rpf'
```

Use uma **cópia isolada** do cache mais recente. Ajuste o número de workers à sua máquina. `warm --no-pipelines` só traduz, sem criar PSOs host.

Resultado observado em uma execução:

- 25.636 shaders traduzidos; 58 recusas de tradução;
- 1.675 pipelines compute processadas, nenhuma recusa de compute;
- 920 PSOs criadas e 755 carregadas;
- biblioteca de 284.404 para 417.556 bytes;
- segunda passagem em 5,3 segundos, sem alterar SHA-256 ou timestamp da biblioteca.

Esses números são de um arquivo e ambiente específicos. A varredura também tem limites: arquivos acima do limite de tamanho aceito pelo leitor (2 GiB neste checkout) podem ser ignorados, e nem todos os estágios são aquecidos. **Não significam que todos os shaders foram suportados**, nem que todas as PSOs gráficas foram compiladas. Reutilização efetiva no jogo deve ser conferida nos logs, não apenas pelo tamanho do cache.

## 7. Por que não pré-criar todas as pipelines gráficas offline?

**Problema:** uma PSO gráfica depende do pareamento VS/MS + PS, root signature e estado real: blend, rasterização, depth/stencil, formatos de render targets, samples, topologia e input layout.

**Contorno disponível:** executar a rota do jogo com cache preservado. A lista `early-pipelines.txt` permite iniciar mais cedo pipelines já conhecidas quando o título as cria; não contém todos os estados necessários para reconstruí-las offline.

**Pendente:** capture/replay do estado gráfico real poderia pré-criar PSOs já observadas. Isso exige implementação e testes; não existe uma solução geral de “100% offline” demonstrada nestes testes. Não basta encaminhar as PSOs sintéticas de `translate --pipelines` para a biblioteca.

## 8. Cache antigo sobrescrevendo descobertas novas

**Problema observado:** um pré-cache gerado a partir de um run antigo não inclui necessariamente as PSOs gráficas de um run posterior. Copiar o blob antigo por cima do novo pode perder trabalho.

**Solução operacional:**

1. Parar o jogo e qualquer processo que use o cache.
2. Copiar o cache mais recente para um novo diretório de experimento.
3. Executar `warm` nessa cópia, de forma aditiva.
4. Usar essa mesma linhagem nos runs seguintes.
5. Registrar hashes antes/depois e conferir `host pipeline library loaded`, `saved` e `rejected`.

Não concatenar nem tentar mesclar manualmente `host-pipelines.bin`. A biblioteca é um blob do driver. Diferenças no conjunto de `early-pipelines.txt` são pistas, não prova exata do conteúdo da biblioteca. Sua ordem pode mudar porque é serializada a partir de um conjunto; compare conjuntos, não apenas o hash textual.

## 9. Cache inválido, mapeado ou incompleto após encerramento

**Problema:** a biblioteca depende de adapter/driver e do código traduzido. Mudanças podem invalidá-la. O código também documenta um problema histórico de páginas mapeadas lendo zeros após substituição do arquivo sem manter o handle, associado a device removal.

**Contorno:** manter os defaults atuais de gerenciamento da biblioteca; não desativar opções de map/handle/trim indiscriminadamente. Nunca sobrescrever o cache enquanto ele estiver em uso. Se aparecer `library rejected`, preservar a cópia problemática para diagnóstico, sem sobrescrever a origem.

Os saves são periódicos, sujeitos às condições do código. Matar o processo pode perder PSOs ainda não salvas; conferir a última mensagem `saved` antes de finalizar. Tamanho crescente sozinho não prova integridade. Comparações por caminho+tamanho também não substituem hashes de conteúdo.

## 10. `VICTORY_PIPELINE_WAIT_MS` baixo ainda permite pausas longas

**Problema:** reduzir apenas esse orçamento não elimina a janela de espera ampliada de carregamento.

**Explicação:** o código combina o orçamento base com pisos de até 4 s para graphics e 12 s para compute enquanto há orçamento de carregamento restante (`VICTORY_PIPELINE_GATE_S`, default 120). O gate não deve ser interpretado simplesmente como “120 segundos após abrir o jogo”.

**Contorno experimental:** estudar o gate em conjunto com o orçamento de espera. Reduzi-lo pode deixar draws/dispatches sem pipeline pronta, inclusive passes importantes. Isso é um trade-off de correção visual, não otimização gratuita. Não recomendar `WAIT_MS=0` como configuração final sem comparação visual.

## 11. Mesh pipeline recusada com `0x80070057`

**Problema:** houve recusa de PSO mesh mesmo com shaders traduzidos. Esse erro é distinto da recusa de root signature com flags `0xc00`.

**Diagnóstico:** verificar suporte real a mesh/SM6.6, stream da PSO, topologia e estado gráfico. Tradução bem-sucedida não garante criação bem-sucedida da PSO.

**Estado:** causa e correção geral ainda pendentes. Aquecer o cache não corrige automaticamente uma PSO inválida. Não ocultar a falha usando `VICTORY_SKIP_PIPELINES` como se fosse uma solução; essa opção descarta trabalho de renderização.

## 12. Seletor de conta e coexistência com gamepad

**Problema observado:** alguns boots ficaram no seletor de conta. Foram testados teclado programado e um segundo controle virtual; também ocorreram crashes em fluxos de entrada forçada.

**O que NÃO foi provado:** que o gamepad físico causou esses crashes. Em outro run ele conectou durante a execução e o jogo continuou sem novo crash. Desconectá-lo para isolar uma variável de teste não é requisito permanente de uso do fork.

**Caminho existente:** no build diagnostics, `VICTORY_KEYS` combina botões programados com os do pad físico slot 0 por OR, preservando o estado físico. Sem pad físico, há o caminho fallback.

**Pendente:** automação confiável de seleção de conta e navegação, convivendo com o usuário. Não desativar dispositivos do Windows nem exigir remoção de bateria como suposta correção comprovada.

## 13. Enter programado durante carregamento ou cena 3D

**Problema:** `VICTORY_KEYS` é uma agenda fixa, interpretada uma vez. O início de seu relógio é o primeiro uso da entrada, não necessariamente o lançamento do processo. Uma agenda que funcionou em um run pode atingir uma cena 3D em outro.

**Contorno seguro:** evitar sequências cegas; reconhecer positivamente a tela, emitir uma entrada limitada e verificar a transição. Parar entradas de menu ao entrar em 3D. Não aceitar ações destrutivas de menus.

**Proposta ainda não implementada:** canal diagnóstico de comando pontual, hold limitado, TTL, proteção contra repetição e confirmação de entrega HLE. Falta de confirmação deve suspender novas entradas, não provocar reenvio cego. Confirmação HLE não prova que o título consumiu o comando; a imagem precisa confirmar a transição.

## 14. “1080p” configurado, mas swapchain continua em 4K

**Problema observado:** `VICTORY_DISPLAY_MODE=1920x1080` e alteração de largura/altura em um settings isolado não demonstraram mudança da swapchain. O log continuou informando `width=3840 height=2160`.

**Solução de diagnóstico:** separar resolução anunciada ao título, resolução interna, swapchain e dimensões do PNG salvo. Uma captura 1920×1080 pode ser redimensionada e não comprova renderização nativa nessa resolução.

**Estado:** redução real da resolução não foi confirmada. Tempos de carregamento menores nesses testes não podem ser atribuídos causalmente a “1080p”.

## 15. Processo vivo por 300 s não é prova de estabilidade

**Problema observado:** o harness antigo iniciava um contador quando `draws >= 500` e o deixava correr mesmo se retornasse a menus ou perdesse progressão. `WORLD_WINDOW_REACHED` descreve esse contador, não um selo de estabilidade.

**Correção metodológica:** correlacionar timestamps, contador de presents, hashes/médias de conteúdo, capturas, janela visível, responsividade e logs de erro. Frames com números crescentes podem conter a mesma imagem preta. Mudanças de hash também podem ser apenas animação de menu.

Exemplo auditado: 881 presents, com 813 hashes distintos, ao longo de aproximadamente 314,967 s após o marco do harness; houve um trecho preto repetido de 3,735 s e um intervalo sem novo present de 6,872 s. Não houve novo crash, mas isso não satisfaz uma prova sem pausas de vários segundos. A meta é 300+ s; 315 s era apenas a margem escolhida pelo harness.

Períodos escuros ou planos fixos de cutscene não são automaticamente crashes: devem ser revisados ou marcados inconclusivos. Stalls de vários segundos e amostras hung observados nos testes impedem declarar a meta completa apenas com o tempo transcorrido. Não impor um número mínimo arbitrário de frames como substituto dos 300 segundos progressivos.

## 16. Captura do desktop, captura interna e captura exclusiva da janela

**Problema:** `CopyFromScreen` do desktop pode capturar outras janelas; encontrar um HWND não equivale a capturar seu conteúdo. `PrintWindow` pode produzir preto em aplicações D3D12 flip-model.

**Solução metodológica:** capturas internas comprovam conteúdo da swapchain; para provar a janela visível, adquirir a janela alvo por método apropriado (por exemplo WGC, ou PrintWindow validado), registrar PID/HWND e timestamps e comparar o conteúdo com a captura interna com tolerância e desvio temporal documentados.

**Estado:** captura externa exclusiva e sua correlação ainda precisam ser implementadas/validadas no harness. Não aprovar uma captura preta nem afirmar que WGC já está implementado.

## 17. Hash do arquivo correto, módulo errado carregado

**Problema:** checar a DLL no disco antes do run não garante que essa seja a DLL usada pelo processo. O harness antigo marcava `coreVerified` apenas por encontrar um módulo.

**Correção necessária:** comparar explicitamente caminho, versão e SHA-256 do módulo carregado com o esperado e integrar a comparação ao resultado. Registrar também executável, revisão Git, features, driver e variáveis de ambiente.

## 18. Saves originais e versões de dados

**Problema:** testes podem modificar saves e metadados; o código de save mantém versões e pode remover versões anteriores ao escolher a atual. Mesclar árvores antigas e novas não equivale a restaurar um snapshot.

**Solução operacional:** manter originais intactos; cada experimento recebe uma cópia em diretório novo e usa `VICTORY_DATA_DIR` e `VICTORY_CACHE_DIR` próprios. Gerar manifesto de hashes da origem. Não apagar saves, não aceitar menus destrutivos e não copiar dados enquanto o jogo os usa. Se precisar repetir de um snapshot, criar outro diretório novo em vez de mesclar sobre um já usado.

## 19. Armadilhas de scripts PowerShell e monitoramento

**Problemas encontrados e correções:**

- `$pid`/`$PID` é variável automática: usar nomes como `$gameProcessId`.
- `GetWindowThreadProcessId` retorna `uint`, não `IntPtr`; declarar corretamente antes de usá-lo com `AttachThreadInput`.
- CSV pode conter decimais com vírgula: usar cultura explícita na leitura ou gravar números invariantes. Não comparar timestamps com strings malformadas.
- `EnumWindows`: parar no primeiro match impede verificar unicidade. Enumerar todos e revalidar PID/HWND; isso ainda não substitui captura de imagem.
- Não marcar HWND ausente no boot como um hung confirmado; registrar estados separados.
- Varredura integral de logs a cada amostra custa tempo: preferir leitura incremental.
- Contar a palavra `ERROR` ou `0xc00` por substring gera falsos positivos (endereços de memória também contêm esses valores). Inspecionar mensagens completas e contexto.
- `window closed: suspending the title` significa encerramento do título; não é prova de sucesso e não deve ser confundido automaticamente com AV.
- Para validar `vgamepad`, criar um dispositivo usando a API realmente disponível. Um erro em um atributo inexistente do módulo não prova falha de instalação. Não criar um segundo pad sem necessidade e sem avaliar a mudança de associação de conta.

## Checklist antes de publicar um resultado

- [ ] Build, revisão, features, driver, hashes e ambiente registrados.
- [ ] Saves originais preservados e experimento isolado.
- [ ] Runtime/Agility carregado verificado, não apenas encontrado no disco.
- [ ] Cache com proveniência conhecida; sem substituição enquanto mapeado.
- [ ] Automação não envia inputs cegos e permite uso do gamepad físico.
- [ ] Janela alvo e capturas correlacionadas; conteúdo realmente progressivo por 300+ s.
- [ ] Stalls, hung, erros e períodos inconclusivos reportados, sem escondê-los em médias.
- [ ] Nenhum novo AV, device removed, rejeição relevante de root signature ou GPU host ausente.
- [ ] Diagnóstico separado da prova final; limitações declaradas.

Os testes documentados avançaram até conteúdo 3D/cutscenes e mostraram reutilização de compute pipelines. **A prova final com todos os critérios acima ainda não está concluída.**
