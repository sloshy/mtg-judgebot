---
title: License and attribution
description: The software license, the Fan Content Policy position, and where the data comes from.
sidebar:
  order: 1
---

## The software

mtg-judgebot is licensed under the **GNU Affero General Public License, version 3 or
later**. Running a modified version as a network service, on Discord or over HTTP,
requires making the modified source available to its users. The `LICENSE` and `NOTICE`
files in the repository are the authoritative statements.

## Magic: The Gathering

This is unofficial Fan Content permitted under Wizards of the Coast's
[Fan Content Policy](https://company.wizards.com/en/legal/fancontentpolicy). It is not
approved or endorsed by Wizards of the Coast. Magic: The Gathering, the Comprehensive
Rules, card names, Oracle text and card rulings are © Wizards of the Coast LLC, a
subsidiary of Hasbro, Inc.

The software downloads the Comprehensive Rules from Wizards' site at run time and stores
them in the operator's database. The repository carries only a short excerpt as a parser
test fixture, which remains Wizards' copyright. Rule citations link to the independent
[Yawgatog](https://yawgatog.com/resources/magic-rules/) mirror of the rules.

## Scryfall

Card data, rulings and the card symbol images come from [Scryfall](https://scryfall.com)
under its [data guidelines](https://scryfall.com/docs/api). Scryfall is not affiliated with
this project. The ingest tooling identifies itself with a User-Agent, uses the bulk-data
endpoint rather than per-card requests, does not download bulk files in parallel, and
paces its symbol requests as Scryfall asks. Card citations link to the card on Scryfall.

## Everything else

The Contributor Covenant (`CODE_OF_CONDUCT.md`) is © its authors, licensed CC BY 4.0. The
Rust and JavaScript dependencies carry their own licenses, recorded in `Cargo.lock` and
`web/package-lock.json`.
