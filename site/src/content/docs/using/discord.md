---
title: Discord commands and ratings
description: The three slash commands, how to phrase a question, what the buttons do, and what a rating changes.
sidebar:
  order: 1
---

## `/judge question:`

Ask in plain language. Nicknames work ("bob", "goyf", "snappy", "t3feri"); the alias list
is `data/aliases.yaml` in the repository and pull requests adding to it are welcome. Write
`[[Full Card Name]]` to pin a card exactly. Answers take twenty to forty-five seconds; the
bot acknowledges at once and edits the reply in.

The reply opens with a non-pinging `@you asked:` header, then the ruling, then a citation
per line. Rule citations link to the Yawgatog mirror of the Comprehensive Rules at the
exact rule; rulings and Oracle text link to the card on Scryfall. The last line carries
the model's confidence and the CR version it answered from.

If a name could mean several cards ("Tibalt", "Emrakul") you get a **did you mean…?** row
of up to five buttons instead of a guess; only the person who asked can pick. If a name
matches nothing, the reply says which and suggests `[[Card Name]]`. Tournament-policy and
price questions are declined after the cheap classification step, before the expensive
synthesis call.

Ask a follow-up in the same channel and the bot sees the recent question-and-answer pairs
from that channel as history, so "what if it had flash?" works.

## Rating buttons

Under every answer: **Incorrect**, **Partially correct**, **Correct**. Rating again
replaces yours. A rating changes one thing: which past answers are shown to the model as
*examples* when a similar question comes in. Scores are smoothed (a Bayesian mean with a
prior of "partially correct" and a weight of three votes) so one early vote cannot swing
an answer's standing; answers rated below 1.5 with at least five votes are excluded.

Members holding the server's judge role (`JUDGE_ROLE`, default `Judge`) rate with an
override: the most recent judge rating replaces the crowd's score for that answer. The
rules always outrank examples: prior answers are rendered after the CR material, labeled
with their rating, and the model is told they are precedent, not authority.

A past answer is retired automatically when its citations stop holding against the
current rules, rulings or Oracle text (a new CR release, an erratum), and comes back if
the text is restored.

## `/help`

What the bot does, how to ask, what it stores, and where the source is. Ephemeral: only
you see it.

## `/forget`

Deletes every rating you have recorded and tells you how many there were. Ratings are the
only data tied to your user id; questions are stored against the channel, not the asker.
