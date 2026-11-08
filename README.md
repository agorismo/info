# Info

Rust (Axum) + SQLite, front em HTML/JS puro.

- Etapa 1: contas, login, perfil, seguir
- Etapa 2: catálogo de filmes e discos, notas, reviews, diário, watchlist, listas
- Etapa 3: scrobbler próprio (tokens de API, histórico, "ouvindo agora", estatísticas)
- Etapa 4: pins e boards (upload de imagem ou link, feed de quem você segue, explorar, busca, salvar em boards)

## Rodar

```
cargo run
```

Abra http://localhost:3000. O banco (`app.db`) é criado sozinho na primeira execução. **Mantenha esse arquivo local e não o envie ao GitHub**: ele contém contas e dados dos usuários. O `.gitignore` exclui o banco e seus arquivos auxiliares, uploads, builds e arquivos `.env`.

Variáveis de ambiente (todas opcionais, mas a busca de filmes precisa da primeira):

| Variável | Para que serve |
|---|---|
| `TMDB_API_KEY` | busca de filmes. Chave grátis em themoviedb.org (Configurações > API). Aceita a "API Key" ou o "Read Access Token" |
| `APP_USER_AGENT` | identificação enviada ao MusicBrainz, ex.: `social-app/0.1 (seu-contato)` |
| `PORT` | porta (padrão 3000) |
| `DATABASE_URL` | padrão `sqlite://app.db?mode=rwc` |
| `COOKIE_SECURE` | use `1` quando o app estiver servido por HTTPS; deixe sem definir no localhost HTTP |

PowerShell: `$env:TMDB_API_KEY="SUA_CHAVE"; cargo run`
CMD: `set TMDB_API_KEY=SUA_CHAVE` e depois `cargo run`

Filmes vêm do TMDB e discos do MusicBrainz (capas do Cover Art Archive). Os resultados buscados ficam salvos no banco, então notas e listas continuam valendo mesmo se o serviço externo cair.

## API

Contas e perfil:

| Método | Rota | O que faz |
|---|---|---|
| POST | `/api/register` | cria conta `{username, password}` e já loga |
| POST | `/api/login` | entra |
| POST | `/api/logout` | sai |
| GET / PUT | `/api/me` | perfil logado / edita `{display_name, bio}` |
| GET | `/api/users/:username` | perfil, contagens, se você já segue |
| POST / DELETE | `/api/users/:username/follow` | seguir / deixar de seguir |
| GET | `/api/users/:username/followers` / `following` | listas |
| GET | `/api/search?q=` | busca usuários |

Catálogo:

| Método | Rota | O que faz |
|---|---|---|
| GET | `/api/catalog/search?kind=movie\|album&q=` | busca no TMDB / MusicBrainz |
| GET | `/api/items/:id` | item, média, sua nota, reviews |
| PUT / DELETE | `/api/items/:id/rating` | `{rating: 0.5 a 5, review}` / remove |
| POST / DELETE | `/api/items/:id/watchlist` | entra / sai da watchlist |
| GET | `/api/users/:username/ratings?kind=` | diário do usuário |
| GET | `/api/users/:username/watchlist` | watchlist |
| GET | `/api/users/:username/lists` | listas do usuário |
| POST | `/api/lists` | cria lista `{title, description}` |
| GET / DELETE | `/api/lists/:id` | ver / apagar lista |
| POST | `/api/lists/:id/items` | `{item_id}` |
| DELETE | `/api/lists/:id/items/:item_id` | remove da lista |

## Scrobbler

1. Entre no app, abra **scrobbler** no topo e crie um token (ele só aparece uma vez; no banco fica só o hash).
2. Seu player envia as músicas com esse token:

```
curl -X POST http://localhost:3000/api/scrobble \
  -H "Authorization: Bearer SEU_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"artist":"Radiohead","track":"Idioteque","album":"Kid A"}'
```

| Método | Rota | O que faz |
|---|---|---|
| POST | `/api/scrobble` | `{artist, track, album?, duration?, timestamp?}`. `timestamp` em segundos Unix (padrão: agora). Repetir o mesmo artista, faixa e timestamp é ignorado |
| POST | `/api/scrobble/batch` | lista de até 100 scrobbles; entra tudo ou nada. Bom pra sincronizar o que foi ouvido offline |
| POST | `/api/nowplaying` | `{artist, track, album?}`; some depois de 10 min sem atualizar |
| GET | `/api/users/:username/scrobbles?limit=&before=` | histórico (mais recentes primeiro) |
| GET | `/api/users/:username/nowplaying` | o que está tocando, ou `null` |
| GET | `/api/users/:username/stats?period=7d\|30d\|90d\|365d\|all` | totais, top artistas/faixas/álbuns e scrobbles por dia (UTC) |
| GET / POST / DELETE | `/api/tokens`, `/api/tokens/:id` | gerenciar tokens (só pela sessão do navegador) |

Convenção de scrobble (a mesma do Last.fm): envie quando a faixa tocou por pelo menos metade da duração ou 4 minutos, e use `nowplaying` quando ela começar.

## Pins e boards

Pins são imagens (enviadas do seu computador ou por link) com título, descrição e link de origem. Boards são coleções: qualquer pessoa pode salvar o pin de outra no seu próprio board. Os arquivos enviados ficam na pasta `uploads/` (criada sozinha, fora do git); só PNG, JPEG, GIF e WebP até 8 MB, e o tipo é checado pelo conteúdo do arquivo, não pelo nome. SVG é recusado de propósito. Ao apagar um pin, o arquivo é apagado junto, a menos que outro pin use a mesma imagem.

| Método | Rota | O que faz |
|---|---|---|
| POST | `/api/uploads` | corpo = bytes da imagem (`Content-Type: image/*`); devolve `{url}` |
| POST | `/api/pins` | `{image_url, title?, description?, link?}`; `image_url` é o `url` do upload ou um link http(s) |
| GET | `/api/pins?feed=explore\|following&q=&before=` | 30 por página; `before` = id do último pin recebido |
| GET / DELETE | `/api/pins/:id` | detalhe (com seus boards e quantos saves) / apagar (só o dono) |
| GET | `/api/users/:username/pins` | pins de um usuário |
| GET | `/api/users/:username/boards` | boards de um usuário, com até 4 capas |
| POST | `/api/boards` | `{title, description?}` |
| GET / DELETE | `/api/boards/:id` | ver / apagar (os pins continuam existindo) |
| POST | `/api/boards/:id/pins` | `{pin_id}` salva o pin no board |
| DELETE | `/api/boards/:id/pins/:pin_id` | tira do board |

## Próximas etapas

5. Mensagens de áudio
6. Call e tela (WebRTC, com WebSocket pra sinalização)
