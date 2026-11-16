# Codenotch na barra de status do VS Code

Mostra na barra de baixo do VS Code, com cor por nível de uso:

- **Claude** (limite semanal) e **GPT/Codex** (sessão de 5 h / semana)
- **CPU, RAM, GPU, VRAM e SSD** (SSD = atividade do disco, igual ao Gerenciador de Tarefas)
- **Portas de localhost abertas** (ex.: `localhost :3000 :5173`); clique abre no navegador

Verde abaixo de 60 %, âmbar de 60 a 79 %, fundo amarelo a partir de 80 %, fundo vermelho a partir de 95 %
(configurável: `codenotch.alertaAmarelo` e `codenotch.alertaVermelho`).

## Requisitos

- Windows.
- O [Codenotch para Windows](../windows) instalado e **aberto** (pode ficar sem ícone). Ele é quem lê o uso do
  Claude e do Codex e grava em `%APPDATA%\codenotch\*.json`; a extensão só lê esses arquivos e roda
  `codenotch.exe sysmon` para CPU/RAM/SSD. Sem o Codenotch, aparecem só RAM, GPU/VRAM e as portas.
- O Claude Code precisa estar logado na mesma conta; no plano Ultra o semanal pode aparecer 0 % por muito tempo.

## Instalar

Com o arquivo pronto (`codenotch-vscode-0.3.1.vsix`, nesta pasta):

```
code --install-extension codenotch-vscode-0.3.1.vsix --force
```

Depois: Ctrl+Shift+P > "Developer: Reload Window".

Para gerar o `.vsix` de novo, nesta pasta: `npx @vscode/vsce package --allow-missing-repository --skip-license`.

## Configuração

| Chave | O que faz |
|---|---|
| `codenotch.exe` | Caminho do `codenotch.exe` (só vale nas configurações de usuário). Vazio = `%LOCALAPPDATA%\Programs\Codenotch\codenotch.exe` |
| `codenotch.alertaAmarelo` | % em que o item fica amarelo (padrão 80) |
| `codenotch.alertaVermelho` | % em que o item fica vermelho (padrão 95) |

Comandos: "Codenotch: Atualizar agora" e "Codenotch: Abrir porta de localhost".

## Como funciona (resumo)

- CPU/RAM/SSD: `codenotch.exe sysmon 3` em rajadas de 3 s (o programa só entrega as linhas ao terminar).
- GPU e VRAM: contadores `GPU Engine` e `GPU Adapter Memory` do Windows via `typeperf`. Em placa integrada, a VRAM é a
  memória compartilhada com a RAM e o teto é estimado em metade da RAM.
- Portas: `netstat -ano` + `tasklist`; só aparecem processos de desenvolvimento (node, python, java, dotnet, docker...).
- Nada sai do computador: a extensão não faz nenhuma chamada de rede.
