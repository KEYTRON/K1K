# K1K — K1 Kernel

Язык: [English (основной)](README.md) | Русский

K1K — гибридное ядро ОС с нуля для x86_64, построенное на капабилити и
написанное на Rust (`no_std`). Это **не** форк Linux и намеренно не UNIX-like:
цель — взять лучшее из разных семейств:

| Откуда | Что берём |
|--------|-----------|
| Linux | скорость и прагматизм: одно привилегированное ядро, без лишних прослоек между ядром и железом |
| Windows NT / macOS XNU | гибридная структура: маленькое привилегированное ядро плюс подсистемы под надзором |
| seL4 / Fuchsia (Zircon) | капабилити вместо UID/ACL: задача может трогать только те объекты, на которые у неё есть handle |
| MINIX 3 / QNX | самовосстановление: сервисы изолированы в ring 3 и перезапускаются супервизором при падении |
| Redox OS | Rust ради memory-safety прямо в ядре |

Ядро владеет планированием, адресными пространствами, IPC и капабилити.
Всё остальное — драйверы, файловые системы, сеть, GUI — должно работать как
изолированные перезапускаемые сервисы.

## Что уже работает

- Загрузка по протоколу [Limine](https://github.com/limine-bootloader/limine)
  (BIOS и UEFI), лог в COM1 и в текстовую консоль на framebuffer.
- GDT/TSS, IDT с обработчиками исключений; исключение в ядре — паника,
  исключение в ring 3 — убивает только виновную задачу.
- Физическая память (bitmap PMM по карте памяти Limine), маппинг страниц ядра,
  отдельные пользовательские адресные пространства с общей верхней половиной,
  куча ядра.
- Вытесняющий round-robin планировщик (PIT @ 200 Гц), потоки ядра,
  sleep/block/wake.
- Таблицы капабилити (`объект + права`, индексация по слотам) и синхронные
  эндпоинты сообщений с прямой передачей заблокированному получателю.
- Задачи ring 3 через `syscall`/`sysret`; сисколлы: `log`, `exit`, `yield`,
  `sleep`, `send`, `recv`, `info`.
- Поток-супервизор, который убирает мёртвые задачи и пересоздаёт упавшие
  сервисы из образа (с backoff).
- Демо-сервисы (плоские бинарники nasm, встраиваются при сборке): `hello`,
  `ping`/`pong` через IPC-эндпоинты и `flaky`, который на каждой третьей
  итерации разыменовывает NULL и поднимается обратно без перезагрузки.

```
[ flaky] about to dereference NULL...
[ fault] task 6 'flaky' #PF addr=0x0 code=0x4 rip=0x40007d -> killed
[superv] service 'flaky' crashed (code -1) -> restarting (restart #1)
[superv] service 'flaky' back up as task 9 at 1220 ms
[ flaky] flaky service started
```

Архитектура и ABI сисколлов: [docs/ARCHITECTURE.ru.md](docs/ARCHITECTURE.ru.md)
([English](docs/ARCHITECTURE.md)).

## Сборка

Нужны: Rust nightly (`rustup` подхватит из `rust-toolchain.toml`), `nasm`,
`xorriso`, `qemu-system-x86_64`, `make`, `git`.

```sh
make            # ядро + загрузочный ISO в build/k1k.iso
make run        # запуск в QEMU (BIOS), serial на stdio
make run-uefi   # запуск через OVMF
make test       # headless-самотест: код 0, если супервизор перезапустил `flaky`
```

Первая сборка клонирует бинарники Limine в `third_party/limine`.

## Структура

```
kernel/           Rust-крейт ядра (x86_64-unknown-none, build-std)
  src/arch/x86_64 GDT, IDT, PIC/PIT, serial, переключение контекста, вход в syscall
  src/mm          pmm (фреймы), vmm (таблицы страниц / адресные пространства), куча
  src/sched       задачи и планировщик
  src/obj         капабилити
  src/ipc         эндпоинты
  src/syscall     диспетчер + доступ к памяти пользователя
  src/service     описания сервисов и супервизор
  src/console     консоль на framebuffer + макросы логирования
user/             программы ring 3 (nasm) и ABI-заголовок user/lib/k1k.inc
tools/            сборщик ISO, генератор шрифта
limine.conf       конфиг загрузчика
```

## Ближайший план

- APIC/IOAPIC + HPET, запуск SMP.
- Загрузчик ELF для сервисов, настоящий user-space runtime.
- Shared-memory IPC для больших данных; асинхронные уведомления.
- Вынос драйверов из ядра: клавиатура уже просто IRQ → эндпоинт; дальше PCI,
  AHCI/virtio, VFS-сервер.
- Сисколлы деривации/отзыва капабилити.

## Лицензия

MIT OR Apache-2.0.
