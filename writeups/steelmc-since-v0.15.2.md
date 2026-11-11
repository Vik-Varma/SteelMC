# SteelMC: v0.15.2+mc26.2 Baseline and Changes Since

As of 2026-11-11 · Living version: [Claude Docs](https://claude.ai/artifact/8b2xZBMoh2gvJL8mSFafen)

## Summary

Since v0.15.2+mc26.2 (tagged 2026-07-28), SteelMC has merged 297 commits from 63 commit authors, moved from Minecraft 26.2 to 26.3, and shipped six more releases, ending at v0.17.0+mc26.3. The work centres on gameplay that was missing at v0.15.2: mobs, projectiles, vehicles, explosions, food and potions, advancements, statistics and 15 new commands.

SteelMC is a Minecraft Java Edition server written in Rust. It aims to match vanilla behaviour while using multicore hardware: ticks stay synchronous, while chunk generation, lighting, packet handling and chunk sending run off the main tick.

| Measure | v0.15.2+mc26.2 | `master` now |
| --- | --- | --- |
| Version | 0.15.2+mc26.2 | 0.17.0+mc26.3 |
| Target Minecraft | 26.2 | 26.3 |
| Workspace crates | 10 | 11 (adds `steel-flint`) |
| Rust source files | 1,383 | 1,785 |
| Built-in commands | 26 | 41 |
| Entity source files (`steel-core/src/entity`) | 118 | 232 |
| Block behaviour files | 175 | 237 |
| Item behaviour files | 26 | 42 |

Releases in between: v0.15.3 (2026-08-30), v0.15.4 (2026-10-03), v0.15.5 (2026-10-13, last 26.2 build), v0.16.0+mc26.3 (2026-10-13), v0.16.1 (2026-10-24), v0.17.0 (2026-11-06). Across the range, 1,467 files changed (+714,871 / -448,375 lines, much of it regenerated data for 26.3).

## State at v0.15.2+mc26.2

At [v0.15.2+mc26.2](https://github.com/Steel-Foundation/SteelMC/releases/tag/v0.15.2%2Bmc26.2) (commit `c9f6a90`, 2026-07-28), SteelMC was pre-alpha: players could join a persistent world, move, build, use inventories and run commands, but survival gameplay was largely missing. The last changes before the tag were the Menu API (#286), terracotta (#282), faster chunk persistence and a README rewrite (#294).

**Workspace (10 crates).** `steel` (binary), `steel-core` (game logic), `steel-protocol`, `steel-login`, `steel-crypto`, `steel-registry`, `steel-worldgen`, `steel-utils`, `steel-math`, `steel-macros`. Pinned nightly Rust toolchain, licence AGPL-3.0-or-later.

| Area | State at v0.15.2 |
| --- | --- |
| World generation | Most complete system. Parity suite: 7,500 random chunks (2,500 per dimension) match vanilla block for block; no entity spawning. Benchmark: 10,201 fresh Overworld chunks in a median 3.98 s on a Ryzen 9 9950X. |
| Networking | Java Edition protocol, Mojang auth, encryption, compression. |
| Chunks | Persistent generation, loading, saving and lighting; separate `ProtoChunk` / `LevelChunk` / `ChunkAccess` types. |
| Player | Movement, collision, block interaction, inventories, chat, permissions, config. |
| Commands (26) | clear, difficulty, domain, enchant, execute, experience, fly, gamemode, gamerule, give, invsee, kill, list, locate, operator, perms, return, seed, setworldspawn, stop, summon, teleport, tellraw, tick, time, weather. |
| Entities | One mob with behaviour (pig). Non-mob entities: item, experience orb, ender pearl, end crystal, firework rocket, item frame, leash knot, block display, chest minecart. |
| Block entities | Barrel, beehive, comparator, daylight detector, end gateway, end portal, moving piston, potent sulfur, sign. |
| Items with behaviour | Axe, hoe, shovel, bucket, bone meal, compass, flint and steel, honeycomb, mace, shield, ender eye, ender pearl, firework rocket, potions and tipped arrows, food on a stick, signs, player head, among others. |
| Blocks | Behaviour folders for building, coloured, container, decoration, farming, fluid, portal, redstone and vegetation blocks. |

**Known gaps called out by the README:** incomplete survival, few entities with real behaviour, no plugin API, and no compatibility with Paper, Bukkit, Fabric, Forge or NeoForge.

## New features

The headline additions are Minecraft 26.3 support (#691), explosions and TNT, potions, advancements, statistics, hostile-mob foundations, 20 new entity types, over 60 block behaviours and 15 commands. Numbers in brackets are upstream PR numbers.

### Platform and version

- **Minecraft 26.3** (#691), shipped as v0.16.0+mc26.3; 26.3 block transformer for hoes, axes and shovels (#735); shelf mushroom block (#756); `/swing` (#782).
- **Embeddable server:** startup (#651) and the shutdown save sequence (#659) can be called from a library; new shutdown handler (#427).
- **Storage:** in-memory player data backend for tests and mini-game servers (#619, #714); one unified, validated `GameProfile` like vanilla (#761).
- **World config:** `minecraft:single_biome_surface` generator, vanilla's single-biome world type (#849); pregen any world via `WORLD_IDENTIFIER` (#844); simulation distance may be 0, with a warning below 4 (#859).
- **Console:** reverse-i-search; Ctrl+C clears the line before exiting (#292). Docker console support (#360).
- **Builds:** musl build (#291); Nix devshell (#484, #671); worldgen crates build for `wasm32-unknown-unknown` (#807); commit hash embedded at compile time (#442).
- **Protocol plumbing:** `Dispatched` write strategy for data-carrying enums (#687); custom per-argument command suggestions (#589, #677).

### Gameplay systems

- **Explosions:** vanilla-compatible destructive explosions, TNT and primed TNT (#428); bed and respawn anchor explosions (#821); end crystal explosions and explosions tripping levers (#824).
- **Food and potions:** foundations (#411), splash potions (#710), lingering potions and area effect clouds (#817).
- **Progression:** statistics (#295, #470, #471); advancements (#688) with saving and loading (#809).
- **Crafting and enchanting:** recipe rework (#565), enchanting table (#623), grindstone (#509).
- **Mob AI:** mob complexity reduction (#306); `NearestAttackableTargetGoal` and `HurtByTarget` (#367); hostile-mob foundations: targeting, pathfinding, combat, equipment, spawning, daylight burning, conversion (#611); mob item pickup (#511).
- **Sleeping and respawn:** sleeping, beds as respawn points, respawn anchor charging (#232).
- **Player:** capes and outer skin layers (#383); adventure-mode item restrictions (#472); idle-kick system (#372); vanilla mining-speed modifiers (#820); drop items from the Creative menu (#528).
- **Leashing:** `Leashable` trait and lead item (#355); each mob decides if it can be leashed (#696).
- **Events:** game events with a source entity (#583); water-entry splash events (#792); entity `first_tick` flag (#560).

### Entities

| Type | Entities added |
| --- | --- |
| Passive mobs | Cow (#303), sheep (#334), chicken with eggs (#429), mooshroom (#590), squid (#693) |
| Ambient | Bat (#698) |
| Hostile | Endermite, spawned by ender pearls (#430) |
| Projectiles | Snowball (#530), fishing hook (#255), eye of ender that finds strongholds (#409), thrown egg, splash and lingering potions |
| Vehicles | Minecart (#749), spawner minecart (#854) |
| Technical | Marker (#539), interaction (#558) |
| Other | Falling block (#333), primed TNT (#428), area effect cloud (#817) |

Entity code was reorganised into `mobs/` and `objects/` trees (#287), and a registry of implemented entities was added for test tooling (#748).

### Blocks

| Group | Blocks with new or completed behaviour |
| --- | --- |
| Plants and growth | Kelp and growing plants (#281), pumpkin and melon stems (#331), small dripleaf (#315), sea pickle (#352), lily pad (#374), chorus growth (#399), glow lichen spreading (#405), nether fungus growth (#407), mangrove growth and roots (#404), grass and mycelium spreading (#650), nylium from bone-mealed netherrack (#438), multiface and double plants (#327), moss carpet (#321), huge mushrooms (#406), mushroom blocks (#369), flower pot (#707) |
| Terrain | Ice (#285), frosted ice (#504), mud (#299), dirt path (#283), snow layer (#280), soul sand (#451), dripstone falling (#461) and fluid transfer (#300), falling blocks (#333), turtle egg (#467) |
| Decoration | Carpets (#284), end rod (#320), lantern (#375), heavy core (#376), pumpkin (#268), carved pumpkin (#432), dragon egg gravity (#395), skulls (#498), signs completed (#786), scaffolding (#491), bell (#425) |
| Containers and stations | Ender chest (#254), shulker box (#298), chiseled bookshelf (#344), jukebox (#350), brushable blocks (#258), beacon (#447), conduit (#398, #825), grindstone (#509), enchanting table (#623), spawner (#712), cake (#708) |
| Other | Lightning rod and weathering lightning rod (#510), bubble column first tick (#607), experience-dropping blocks (#330), TNT (#428), potent sulfur / candle cake / spore blossom TODOs (#620) |

### Items

Name tag (#341), water bottle on dirt to make mud (#290), spyglass (#415), ink and glow ink sacs (#458), lead (#355), powder snow bucket (#424), glass bottle filling (#531), snowball (#530), shears (#462), fishing rod (#255), spawn eggs (#412), totem of undying (#512), compass lodestone binding (#506), shovel flattening sound (#468), `ItemStack::consume` (#587), item-to-block mapping on `Item` (#719).

### Commands (15 new)

| Command | PR |
| --- | --- |
| `/advancement` | #688 |
| `/damage` | #296 |
| `/defaultgamemode` | #704 |
| `/fill` | #443 |
| `/playsound` | #593 |
| `/setblock` | #301 |
| `/setidletimeout` | #372 |
| `/spawnpoint` | #456 |
| `/stopsound` | #683 |
| `/swing` | #782 |
| `/tag` | #626 |
| `/title` | #588 |
| `/transfer` | #731 |
| `/version` | #680 |
| `/worldborder` | #459 |

`/domain` also gained `/tp`-style location and rotation arguments (#763).

## Bug fixes

The most important fixes harden the network edge against malformed or hostile clients, and stop chunk-ticket and save races that could lose or corrupt data. The rest bring gameplay closer to vanilla.

### Security and robustness

- Server crashes from invalid use-item-on locations (#324) and invalid use-item rotations (#322).
- Removed a PKCS#1 v1.5 padding oracle from login key handling (#699).
- Chat: session fields bounded before allocation (#329); chat profile keys validated against Mojang's service keys (#332).
- Compressed packet decompression bounded (#342); `SSelectKnownPacks` limited to 64 entries (#394).
- Pre-play packet sequencing enforced (#336); clients disconnected when Login/Config packets fail to decode (#645).
- `online_mode` no longer ignored when encryption is on (#397); `max_players` enforced on join (#686).
- Ghost players after stalled disconnects prevented (#370); non-finite rotations no longer crash `/domain` (#763).

### Chunks, saving and ticking

- Chunk ticket timing and ownership match vanilla (#577); ticket revival wins over an in-flight save (#610); valid tickets kept when saved data has bad entries (#655).
- Chunk ACK races no longer corrupt send pacing (#393).
- Global palette bit widths derived from the registry (#660); saving no longer silently writes the wrong block for a broken palette (#777).
- Living-entity equipment persists across chunk and vehicle saves (#582).
- Overloaded server skips the tick backlog with "Can't keep up" instead of fast-forwarding the world (#617).
- Game time shared across worlds in one domain (#675); `/domain` no longer spawns players over lava or on the Nether roof (#763).

### Players, mobs and combat

- Respawn: abilities restored (#338); stale client entities removed (#410); fresh player objects for death and End respawns (#578); dead players keep ticking after entity removal (#840) and can no longer drop items during the death animation (#857).
- Living-entity hurt and death handling aligned with vanilla (#625); death knockback and item pickup delay on mob drops (#758).
- Damage-source entity references survive removal (#692); projectile owners resolved across worlds in a domain (#737); fire aspect and looting apply to mob drops (#694).
- Sprint exhaustion charged once per movement (#653); gliding velocity synced for all entities (#636).
- Mob AI: waypoint distance measured from the mob (#634); line of sight checked for every mob (#635).
- Breeding drops one XP orb (#724); sheep colour RNG (#752); babies from eggs for non-ageable mobs (#800); item pickup at the simulated player position (#801).
- Sounds and effects: melee attack (#836), equip (#790), item step (#802), landing particles (#787), fall hurt animation suppressed (#789), fishing rod vibrations (#793).
- `ITEM_CRAFTED` stat awarded from crafting and furnace output (#833).

### Blocks and items

- Fire behaviour (#441); multiface attach checks no longer offset twice (#413); snowy state updated on grass, podzol and mycelium (#380).
- Block drops for face-directed items (#357); container drops on destroy and the `build.tooHigh` message (#431).
- Pick block (#389) and picking the right item when block and item IDs differ (#502).
- Swords no longer break blocks in Creative (#637); `use_on_release` added (#469); candle stacking (#709).
- Wall torches and ceiling hanging signs (#772); correct level event ID (#773); vanilla `Mth` maths functions (#805); NBT numeric handling (#827).

### Commands

- `/setblock`: air destroy mode (#388), inverted `keep` mode (#416), failure after the block-state parser change (#585).
- `/damage` attacker attribution (#381); `setrandomtickspeed` gamerule (#433); duplicate dimension suggestions (#754); translated messages (#822).

## Performance improvements

The biggest wins are in world generation (#823: about 29% faster chunk generation; #594: 55% fewer allocations) and network encoding (#777: about 5x faster chunk-section encoding). Figures below are the PR authors' own measurements, often microbenchmarks, not server-wide TPS.

| PR | Area | Change | Reported result |
| --- | --- | --- | --- |
| [#823](https://github.com/Steel-Foundation/SteelMC/pull/823) | Worldgen | Skips unneeded work; shares cached noise and surface data across chunks | ~29% faster chunk generation; hash parity test passes |
| [#594](https://github.com/Steel-Foundation/SteelMC/pull/594) | Worldgen | Allocation cuts | 15.1M to 6.7M allocations (-55.7%); 3.66 GB to 2.90 GB allocated |
| [#624](https://github.com/Steel-Foundation/SteelMC/pull/624) | Worldgen | Removes an unneeded chunk-gen step | ~30% fewer allocations |
| [#479](https://github.com/Steel-Foundation/SteelMC/pull/479) | Worldgen | Sparse per-chunk cache for ore placement instead of a dense 17x17 cache per attempt | ~24% less memory allocated; -6.6% pregen time; 0 parity mismatches |
| [#485](https://github.com/Steel-Foundation/SteelMC/pull/485) | Worldgen | Reuses y/x trilinear-interpolation partials across z columns | ~21 to ~9 vector ops per block; bit-identical output |
| [#478](https://github.com/Steel-Foundation/SteelMC/pull/478) | Worldgen | Caches parsed vanilla structure templates (`Arc`, process-wide) | Templates parsed once instead of per placed piece |
| [#481](https://github.com/Steel-Foundation/SteelMC/pull/481) | Lighting | Thread-local pool for light propagation queues | Removes ~64 KiB of allocation per chunk light pass |
| [#777](https://github.com/Steel-Foundation/SteelMC/pull/777) | Network | Rewrites `PalettedContainer::write`: lookup table, const-generic bit packing | ~5x faster section encoding, byte-identical output; also fixes a save bug |
| [#796](https://github.com/Steel-Foundation/SteelMC/pull/796) | Network | Bulk byte-array writes; identifiers written without a temp string | 64 KiB array: 65.2 µs to 1.07 µs |
| [#783](https://github.com/Steel-Foundation/SteelMC/pull/783) | Network | VarInt/VarLong bulk writes, BMI2 `pdep` path on x86-64 | 46-79% faster on multi-byte values; single-byte up to 8% slower |
| [#349](https://github.com/Steel-Foundation/SteelMC/pull/349) | Network | Inbound packets keep their buffer instead of copying the payload | 1-2 fewer allocations per packet (up to -66% bytes) |
| [#550](https://github.com/Steel-Foundation/SteelMC/pull/550) | Network | Tab list for a joining player sent as one `CPlayerInfoUpdate` | One packet instead of O(N) |
| [#414](https://github.com/Steel-Foundation/SteelMC/pull/414) | Chunk sending | Single forward cursor in `commit_batch` | Quadratic to linear per batch |
| [#482](https://github.com/Steel-Foundation/SteelMC/pull/482) | Chunk saving | Hash-map palette inversion; streaming bit packing | No per-block linear scans or temp `Vec` |
| [#725](https://github.com/Steel-Foundation/SteelMC/pull/725) | Blocks | Non-allocating block-state property API | Drops two `Vec`s and a `String` per lookup |
| [#311](https://github.com/Steel-Foundation/SteelMC/pull/311) | Redstone | Lithium-style redstone wire signal calculation | Fewer neighbour reads; vanilla behaviour kept |
| [#797](https://github.com/Steel-Foundation/SteelMC/pull/797) | Entities | `SmallVec` passenger-cycle check instead of a `HashSet` per entity per tick | No per-entity tick allocation in the common case |
| [#815](https://github.com/Steel-Foundation/SteelMC/pull/815) | Entities | Cheaper spatial queries, index updates and push contacts | Dense movement 686 ns to 236 ns; pushing 12-18% faster |
| [#763](https://github.com/Steel-Foundation/SteelMC/pull/763) | `/domain` | Spawn search loads each chunk once, in vanilla order | Worst case was 441 ticks (~22 s) |

**Build times.** #852 adds line breaks to generated code so rustc compiles it faster; #858 enables incremental compilation for the release profile; #814 shortens the CI profile; #768 swaps rustfmt for prettyplease on generated files, now opt-in via the `fmt` feature.

## Refactors, tooling and dependencies

The largest structural change is one `Chunk` type replacing `ProtoChunk`, `LevelChunk` and `ChunkAccess` (#307). The largest tooling change is the Flint test harness, now in-tree as the `steel-flint` crate (#788).

### Core refactors

- One `Chunk` struct instead of three chunk types, now that worldgen is mostly done (#307).
- Global registries bootstrapped from one function instead of 9 copies (#314).
- `RawEntity` removed: unsupported entities are skipped on load and placement instead of kept as inert objects (#557).
- Fluid system made generic rather than water/lava-specific, ahead of a modding API (#828).
- `LevelReader` trait objects removed where unneeded (#365); `Animal` methods moved to `AgeableMob` (#657); `BlockAttachedEntity` trait (#474).
- Canonical constants and shared invariants centralised (#538); magic numbers removed (#353); command argument getters return `Result` (#340); vanilla collision-context naming (#493).
- Login: boxed Java socket halves and `JavaTcpClient::from_transport` (#679).
- Codebase-wide `use __::*` imports (#759), stricter Clippy lints (#728, #723), dependency update (#766), newer nightly (#449).

### Testing

- **Flint** harness merged in as `steel-flint` (#788): JSON test specs run against real `steel-core` code with RAM-only worlds and can be compared with vanilla. Supporting work: synchronous chunk processing (#318), shared test-server functions (#715), registry of implemented entities (#748), only run specs whose IDs Steel implements (#810), compile fix (#853).
- Simpler test player creation (#366); vanilla entity dimensions in tests (#648); flaky set-block test fixed (#337); nextest fixed (#371); test init fixed (#722).

### CI and project process

- PR template (#317) with a "classes / commands modified" section (#499, #595).
- Auto-labelling of PRs (#373); `approved-by-<maintainer>` labels (#463); maintainers need one approval on their own PRs (#791); per-contributor open-PR limit and review-time estimate (#705).
- Release notes grouped by label (Blocks, Items, Commands, Entities, Performance, Features, Bugs) via `.github/release.yml` (#832); release zip packaging updated (#839).
- Repo-wide LF line endings (#310, #363); `prek` hook fix (#581); panic hook prints debug info (#703); Git metadata path fix (#755); datapacks required before worldgen build (#778); `cargo run` fix (#811).

## Documentation changes

In-repo documentation grew by two Flint guides and a PR template, and the README, CONTRIBUTING and AGENTS files were updated for 26.3 and new contributor rules. Files changed since v0.15.2:

| File | Change | PRs |
| --- | --- | --- |
| `README.md` | Target version 26.2 to 26.3; new-contributor limits; Nix setup (`nix develop` / `.envrc`); opt-in `fmt` feature for generated code; pointer to `good first issue` and the tracker; GitHub Sponsors link | #305, #484, #705, #727, #768, #781 |
| `CONTRIBUTING.md` | New "New Starters" (max 3 open PRs, start under ~1k lines) and "Review time" (ask after a week) sections; new Testing section pointing to Flint; Parchment-mappings rule removed | #705, #788 |
| `AGENTS.md` | Ticks must never wait on async work or poll in a loop; rules on when to cite vanilla names in comments; CI Clippy command now uses `--workspace` | #307, #711, #853 |
| `.github/PULL_REQUEST_TEMPLATE.md` | New: change-type checklist, testing, checklist, classes/commands modified | #317, #373, #499, #595, #808 |
| `.github/release.yml` | New: changelog categories for GitHub releases | #832 |
| `steel-flint/README.md`, `steel-flint/GETTING_STARTED.md` | New: what the Flint adapter provides, how it works, setup and running specs | #788, #810 |
| `.github/workflows/docs.yml` | Rustdoc workflow fix | #475, #766 |

Vanilla-mirroring doc comments were stripped across the code (#711). Contributor docs on the website were updated alongside #705 in the separate SteelDocs repo (SteelDocs PR #78).

## Sources

Range covered: tag `v0.15.2+mc26.2` (`c9f6a90`, 2026-07-28) to upstream `master` at `fb85bdc` ("Remaining explosion work", #824, 2026-11-11), 297 squash-merged commits. Built from `git log` / `git diff` on a full clone of upstream and from the descriptions and labels of the merged PRs. Performance figures are as reported in each PR.

- [Release v0.15.2+mc26.2](https://github.com/Steel-Foundation/SteelMC/releases/tag/v0.15.2%2Bmc26.2)
- [All releases](https://github.com/Steel-Foundation/SteelMC/releases)
- [Merged pull requests](https://github.com/Steel-Foundation/SteelMC/pulls?q=is%3Apr+is%3Amerged)
- [Compare v0.15.2+mc26.2...master](https://github.com/Steel-Foundation/SteelMC/compare/v0.15.2%2Bmc26.2...master)
- [SteelMC implementation tracker](https://steelmc.dev/tracker)
