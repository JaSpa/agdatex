MAIN_TEX=paper.tex
MAIN_PDF=paper.pdf
FAST_PDF=paper_fast.pdf
SRC_DIR=STLC
CACHE_DIR=_cache
EXTRA_FILES=$(wildcard *.bib *.cls *.sty *.tex)

SRCS=$(wildcard $(SRC_DIR)/*.agda)
TGTS=$(patsubst $(SRC_DIR)/%.agda,$(CACHE_DIR)/$(SRC_DIR)/%.tex,$(SRCS))

.PHONY: all paper fast

all: paper

# Run pdflatex only once
fast: $(FAST_PDF)

# Run pdflatex, then bibtex, then two times pdflatex
paper: $(FAST_PDF)
	cd $(CACHE_DIR); bibtex main; pdflatex main.tex; pdflatex main.tex
	cp $(CACHE_DIR)/main.pdf $(MAIN_PDF)

$(FAST_PDF): $(TGTS) $(MAIN_TEX) $(EXTRA_FILES)
	cp $(MAIN_TEX) $(CACHE_DIR)/main.tex
	cp -f $(EXTRA_FILES) $(CACHE_DIR)/
	cd $(CACHE_DIR); pdflatex main.tex
	cp $(CACHE_DIR)/main.pdf $(FAST_PDF)

$(TGTS): ../agdatex.py $(SRCS)
	../agdatex.py -r . -o $(CACHE_DIR) $(SRCS)
