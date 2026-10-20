# Security policy

monkeytrading is meant to be trusted without trusting the people who run it: its rules are public
code, it runs inside a confidential-computing enclave, and anyone can replay the commands it
publishes to check the state it reports. A vulnerability can undermine any of that, so we ask that
you report it privately and give us time to fix it before it becomes public. This policy explains
how to reach us, what we commit to in return, and what it covers.

## Reporting a vulnerability

Email reports to william.jevons@proton.me. Please do not describe a vulnerability in a public
issue, pull request, or discussion, even in outline. Mail sent from a Proton account is end-to-end
encrypted; a PGP key for senders on other providers will be published in this file.

A report is easiest to act on when it names the affected component and the commit of `main` you
tested, says what an attacker can achieve and what access they need first, and shows how to
reproduce it. A failing test, a command log that replays to a different state from the one the node
reported, or a script against a local node makes the strongest case. Tell us, too, whether you have
a fix in mind, whether anyone else knows of the issue, and how you would like to be credited.

## What happens next

We aim to acknowledge a report within three business days. Within ten business days of receiving
it, we will tell you whether we can reproduce the issue, how severe we judge it, and what we plan to
do. We aim to fix a vulnerability that puts funds, signing keys, or the integrity of the published
log at risk within 30 days of confirming it, and any other within 90 days, and we will keep you
informed until it is resolved. If we conclude that a report does not describe a vulnerability, we
will explain why.

## Coordinated disclosure

We ask that you keep the details of a vulnerability private until a fix has shipped, or until 90
days after you reported it, whichever comes first. If a fix needs longer, we will tell you why
before the deadline and agree a new date with you. Once the fix has shipped, we publish an advisory
in this repository that describes the vulnerability, its impact, and the fix, and credits you by
name unless you would rather it did not. You are free to publish your own account from the same
day. If we conclude that a report does not describe a vulnerability, you may disclose it once we
have told you so.

## Safe harbour

We will not take or support legal action against anyone who researches and reports a vulnerability
in good faith under this policy, and if a third party does, we will make it known that the research
was authorised. Good faith means testing against a node you run yourself, or against accounts and
funds you own; not accessing, changing, or destroying anyone else's data or funds beyond the minimum
needed to show the problem; not degrading the service for other users; and keeping the details
private as this policy asks. If you are unsure whether something you plan to do is covered, ask us
before you do it.

## Scope

The policy covers the code in this repository and what is built from it: `engine-core`, the
deterministic state machine that applies the trading rules; `engine-node`, which sequences commands
into it, publishes the log and its signed checkpoints, and serves the public REST and WebSocket API;
the custody vault contract that holds collateral on Arbitrum One; and the enclave image the node
runs in, together with the attestation that vouches for it. The vault contract and the enclave image
are still being built, and each is in scope as soon as its code is published.

The findings that matter most let someone take funds they are not owed, fill an order at a price the
rules do not allow, make the state the node reports differ from the state its published log replays
to, forge or reuse a signature the enclave makes, read or alter what the enclave holds from the
machine it runs on, or halt the engine with a request any client can send.

The mock enclave, mock custody, and simulated prices exist for local development and make no
security claims, so a weakness in them is in scope only if a production configuration can reach it.
The same holds for development-only endpoints such as `POST /v1/dev/deposit`, which the node serves
only with mock custody.

Out of scope are volumetric denial of service, meaning attacks that work only by flooding the API or
the network with traffic; social engineering of anyone involved with the project; and
vulnerabilities in third-party venues and services, such as the exchanges that supply prices, the
Arbitrum network, the USDC contract, and the enclave hardware, which should be reported to their
owners. How the engine handles what those services send it, including a venue that quotes a false
price, is in scope.

Findings that need an operator's key or credentials to be compromised first are also out of scope,
unless they defeat a guarantee the engine makes. The engine is designed so that whoever runs it, and
controls the machine it runs on, cannot alter the published log without detection, have the enclave
sign a state its rules do not produce, or pay out funds the engine did not approve. A finding that
breaks one of these guarantees is in scope even when it starts from an operator's key.

## Supported versions

monkeytrading has not launched and has no releases. Until the first release, only the latest commit
on `main` is supported, so please check that an issue still reproduces there before reporting it.

## Bug bounty

A bug bounty, with its scope and rewards, will be announced before launch.
