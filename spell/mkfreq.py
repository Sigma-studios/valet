"""Writes <filters>/freq.txt: form \t rank 0..25 (morfologik scale) from wordfreq Zipf values."""
import sys
from wordfreq import zipf_frequency
lang, filters = sys.argv[1], sys.argv[2]
n = 0
with open(f"{filters}/speller.txt", encoding="utf-8") as f, open(f"{filters}/freq.txt", "w", encoding="utf-8") as out:
    for line in f:
        w = line.rstrip("\n").split("\t")[0]
        if not w:
            continue
        z = zipf_frequency(w, lang)
        if z > 0:
            out.write(f"{w}\t{min(25, round(z / 8 * 25))}\n")
            n += 1
print("forms with a frequency:", n)
