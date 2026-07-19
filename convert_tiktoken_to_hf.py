"""
Converts OpenAI's cl100k_base.tiktoken rank table (vendored from
https://github.com/niieani/gpt-tokenizer, MIT licensed) into a
tokenizer.json compatible with the `tokenizers` Rust crate v0.15.x's BPE
model (which expects "merges" as an array of "left right" strings, per
tokenizers-0.15.2/src/models/bpe/serialization.rs).

tiktoken's rank file only lists (byte-sequence -> rank), not the explicit
merge pairs -- the merge sequence has to be *reconstructed* by re-running
BPE merge search on each token's raw bytes restricted to only lower-rank
merges. This is a standard, publicly documented technique (see
https://github.com/openai/tiktoken/issues/60#issuecomment-1499977960 and
the HF community conversion notes referenced in tiktoken/transformers
discussions) -- not something invented here.

This is a one-time, offline build step. Its output (tokenizer.json) is what
actually gets embedded into the Rust binary via include_str!.

To fetch the source data first:
    curl -sL -o cl100k_base.tiktoken \
      https://raw.githubusercontent.com/niieani/gpt-tokenizer/main/data/cl100k_base.tiktoken

Then run this script from the repo root:
    python3 convert_tiktoken_to_hf.py
"""
import base64
import json

SRC = "./cl100k_base.tiktoken"
OUT = "rust-core/resources/tokenizer.json"

# cl100k_base's special tokens and their fixed IDs, as defined in tiktoken's
# own encoding registry (public, documented constants -- not creative
# content, just IDs).
SPECIAL_TOKENS = {
    "<|endoftext|>": 100257,
    "<|fim_prefix|>": 100258,
    "<|fim_middle|>": 100259,
    "<|fim_suffix|>": 100260,
    "<|endofprompt|>": 100276,
}

# cl100k_base's actual pre-tokenizer split regex, as published in tiktoken's
# source and reproduced in every community HF conversion (e.g. the Xenova
# gist referenced in project research) -- again, a functional constant.
CL100K_SPLIT_PATTERN = (
    r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}{1,3}"
    r"| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+"
)


def bytes_to_unicode():
    """Standard reversible byte<->unicode mapping used by GPT-2-style
    byte-level BPE (originally from OpenAI's gpt-2 repo's bytes_to_unicode(),
    reproduced identically in every HF byte-level tokenizer since -- a
    mechanical, deterministic algorithm, not creative content)."""
    bs = (
        list(range(ord("!"), ord("~") + 1))
        + list(range(ord("\xa1"), ord("\xac") + 1))
        + list(range(ord("\xae"), ord("\xff") + 1))
    )
    cs = bs[:]
    n = 0
    for b in range(256):
        if b not in bs:
            bs.append(b)
            cs.append(256 + n)
            n += 1
    cs = [chr(c) for c in cs]
    return dict(zip(bs, cs))


def bpe_split(mergeable_ranks, token, max_rank):
    """Re-derives the two parts a given token was merged from, by running
    greedy BPE merge search on its raw bytes but only allowing merges with
    rank < max_rank (i.e. merges that were established *before* this token
    itself). For a token that came from a real BPE merge, this always
    converges to exactly 2 parts."""
    parts = [bytes([b]) for b in token]
    while True:
        min_idx, min_rank = None, None
        for i in range(len(parts) - 1):
            pair = parts[i] + parts[i + 1]
            rank = mergeable_ranks.get(pair)
            if rank is not None and rank < max_rank and (min_rank is None or rank < min_rank):
                min_idx, min_rank = i, rank
        if min_idx is None:
            break
        parts = parts[:min_idx] + [parts[min_idx] + parts[min_idx + 1]] + parts[min_idx + 2:]
    return parts


def main():
    mergeable_ranks = {}
    with open(SRC) as f:
        for line in f:
            token_b64, rank = line.split()
            mergeable_ranks[base64.b64decode(token_b64)] = int(rank)

    byte_encoder = bytes_to_unicode()

    def token_str(b: bytes) -> str:
        return "".join(byte_encoder[byte] for byte in b)

    vocab = {}
    merge_lines = []  # list of (rank, "left right") so we can sort by rank

    for token_bytes, rank in mergeable_ranks.items():
        vocab[token_str(token_bytes)] = rank
        if len(token_bytes) > 1:
            parts = bpe_split(mergeable_ranks, token_bytes, rank)
            assert len(parts) == 2, f"expected 2 parts for rank {rank}, got {len(parts)}"
            merge_lines.append((rank, f"{token_str(parts[0])} {token_str(parts[1])}"))

    merge_lines.sort(key=lambda x: x[0])
    merges = [m for _, m in merge_lines]

    for tok, tid in SPECIAL_TOKENS.items():
        vocab[tok] = tid

    tokenizer_json = {
        "version": "1.0",
        "truncation": None,
        "padding": None,
        "added_tokens": [
            {
                "id": tid,
                "content": tok,
                "single_word": False,
                "lstrip": False,
                "rstrip": False,
                "normalized": False,
                "special": True,
            }
            for tok, tid in SPECIAL_TOKENS.items()
        ],
        "normalizer": None,
        "pre_tokenizer": {
            "type": "Sequence",
            "pretokenizers": [
                {
                    "type": "Split",
                    "pattern": {"Regex": CL100K_SPLIT_PATTERN},
                    "behavior": "Removed",
                    "invert": True,
                },
                {
                    "type": "ByteLevel",
                    "add_prefix_space": False,
                    "trim_offsets": True,
                    "use_regex": False,
                },
            ],
        },
        "post_processor": None,
        "decoder": {
            "type": "ByteLevel",
            "add_prefix_space": False,
            "trim_offsets": True,
            "use_regex": True,
        },
        "model": {
            "type": "BPE",
            "dropout": None,
            "unk_token": None,
            "continuing_subword_prefix": None,
            "end_of_word_suffix": None,
            "fuse_unk": False,
            "byte_fallback": False,
            "vocab": vocab,
            "merges": merges,
        },
    }

    import os
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(tokenizer_json, f, ensure_ascii=False)

    print(f"wrote {OUT}")
    print(f"vocab size: {len(vocab)}, merges: {len(merges)}")


if __name__ == "__main__":
    main()
