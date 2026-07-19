from reportlab.lib.pagesizes import letter
from reportlab.pdfgen import canvas
from reportlab.lib.units import inch

PAGE_COUNT = 27
OUT_PATH = "/home/claude/rag-ingestion-pipeline/testdata/test_financial_report.pdf"

def draw_header_footer(c, page_num, total_pages):
    width, height = letter
    c.setFont("Helvetica", 8)
    c.drawString(0.75 * inch, height - 0.5 * inch, "ACME Global Holdings — Confidential Financial Report")
    c.drawRightString(width - 0.75 * inch, height - 0.5 * inch, "FY2026 Annual Filing")
    c.drawString(0.75 * inch, 0.5 * inch, f"Page {page_num} of {total_pages}")
    c.drawRightString(width - 0.75 * inch, 0.5 * inch, "(c) 2026 ACME Global Holdings. All rights reserved.")

def draw_body_section1(c, y):
    c.setFont("Helvetica-Bold", 18)
    c.drawString(0.75 * inch, y, "Product X")
    y -= 30
    c.setFont("Helvetica-Bold", 13)
    c.drawString(0.75 * inch, y, "System Requirements")
    y -= 22
    c.setFont("Helvetica", 10)
    lines = [
        "Product X requires a minimum of 16GB of RAM and a quad-core processor",
        "running at 2.4GHz or higher. Storage requirements scale with data volume",
        "but a baseline of 500GB SSD is recommended for production deployments.",
        "Network connectivity must support at least 1 Gbps throughput between",
        "application and database tiers for acceptable query latency.",
    ]
    for line in lines:
        c.drawString(0.75 * inch, y, line)
        y -= 14
    return y

def draw_body_section2(c, y):
    y -= 20
    c.setFont("Helvetica-Bold", 13)
    c.drawString(0.75 * inch, y, "Deployment Notes")
    y -= 22
    c.setFont("Helvetica", 10)
    lines = [
        "Deployment should follow the blue-green rollout strategy described in",
        "the operations runbook. Rollbacks must complete within a 15 minute",
        "service level objective to avoid triggering the incident escalation path.",
    ]
    for line in lines:
        c.drawString(0.75 * inch, y, line)
        y -= 14
    return y

def draw_product_y_section(c, y):
    c.setFont("Helvetica-Bold", 18)
    c.drawString(0.75 * inch, y, "Product Y")
    y -= 30
    c.setFont("Helvetica-Bold", 13)
    c.drawString(0.75 * inch, y, "System Requirements")
    y -= 22
    c.setFont("Helvetica", 10)
    lines = [
        "Product Y is a lightweight companion service requiring only 4GB of RAM",
        "and a single-core processor. It is designed to run on edge devices with",
        "constrained resources and does not require SSD-backed storage.",
    ]
    for line in lines:
        c.drawString(0.75 * inch, y, line)
        y -= 14
    return y

def draw_table(c, y):
    y -= 20
    c.setFont("Helvetica-Bold", 13)
    c.drawString(0.75 * inch, y, "Quarterly Revenue (USD millions)")
    y -= 20
    c.setFont("Helvetica", 10)
    rows = [
        ["Quarter", "Product X", "Product Y", "Total"],
        ["Q1 2026", "12.4", "3.1", "15.5"],
        ["Q2 2026", "14.0", "3.4", "17.4"],
        ["Q3 2026", "15.2", "3.9", "19.1"],
        ["Q4 2026", "16.8", "4.2", "21.0"],
    ]
    col_x = [0.75 * inch, 2.25 * inch, 3.75 * inch, 5.25 * inch]
    for row in rows:
        for cell, x in zip(row, col_x):
            c.drawString(x, y, cell)
        y -= 16
    return y

def draw_ruled_table(c, y):
    # A genuinely RULED table (drawn grid lines via re/l/S operators), as
    # opposed to draw_table() above which only places text with no borders
    # at all. This exercises the vector-operator-based ruled-grid detector
    # (table_grid.rs) rather than the spatial-alignment heuristic.
    c.setFont("Helvetica-Bold", 13)
    c.drawString(0.75 * inch, y, "Regional Headcount (Ruled Table)")
    y -= 24
    top = y
    left = 0.75 * inch
    col_widths = [1.5 * inch, 1.5 * inch, 1.5 * inch]
    row_height = 0.28 * inch
    n_rows = 4
    table_width = sum(col_widths)
    table_height = row_height * n_rows

    c.setLineWidth(1)
    # Outer border + all internal horizontal/vertical rules.
    c.rect(left, top - table_height, table_width, table_height, stroke=1, fill=0)
    for r in range(1, n_rows):
        ry = top - r * row_height
        c.line(left, ry, left + table_width, ry)
    x = left
    for w in col_widths[:-1]:
        x += w
        c.line(x, top - table_height, x, top)

    rows = [
        ["Region", "Headcount", "Open Reqs"],
        ["AMER", "412", "18"],
        ["EMEA", "265", "9"],
        ["APAC", "198", "14"],
    ]
    c.setFont("Helvetica", 10)
    for r, row in enumerate(rows):
        cell_y = top - r * row_height - row_height * 0.65
        cx = left
        for cell, w in zip(row, col_widths):
            c.drawString(cx + 6, cell_y, cell)
            cx += w
    return top - table_height - 10

def draw_two_column_page(c):
    # A genuine 2-column layout with a real gutter, and DIFFERENT text in
    # each column so that any left/right blending in the rendered Markdown
    # (naive top-to-bottom-across-the-page ordering) is immediately visible
    # as garbled, interleaved sentences rather than two coherent paragraphs.
    width, height = letter
    c.setFont("Helvetica-Bold", 13)
    c.drawString(0.75 * inch, height - 1.1 * inch, "Risk Factors Overview")
    y = height - 1.4 * inch
    left_x = 0.75 * inch
    right_x = 4.65 * inch  # comfortable ~35-45pt gutter even for the widest left-column line

    left_lines = [
        "Market risk arises from currency exchange rate",
        "fluctuations across the regions where the company",
        "operates its manufacturing and distribution sites.",
        "Management monitors exposure using rolling hedges",
        "placed on a quarterly basis to reduce reported",
        "earnings volatility from period to period.",
        "These hedges are reviewed by the treasury team.",
    ]
    right_lines = [
        "Operational risk includes disruption to the supply",
        "chain from single-source component vendors located",
        "in regions subject to periodic regulatory changes.",
        "The company maintains dual-sourcing agreements for",
        "its highest-volume components to reduce this risk.",
        "Contingency logistics plans are tested twice per",
        "year with each regional operations team lead.",
    ]
    c.setFont("Helvetica", 9.5)
    yy = y
    for line in left_lines:
        c.drawString(left_x, yy, line)
        yy -= 13
    yy = y
    for line in right_lines:
        c.drawString(right_x, yy, line)
        yy -= 13

def draw_garbled_page(c):
    # Simulates a page a naive parser would mangle: overlapping / rotated
    # text plus content that reads as "word salad" -- used to exercise the
    # S_fidelity < 0.70 quarantine gate.
    width, height = letter
    c.setFont("Helvetica", 9)
    y = height - 1.2 * inch
    salad = "erm ipsum revenue Q3 misaligned column drift lorem token noise fragment"
    for i in range(4):
        c.drawString(0.75 * inch + (i % 3) * 5, y, salad)
        y -= 11

def main():
    c = canvas.Canvas(OUT_PATH, pagesize=letter)
    width, height = letter

    for page_num in range(1, PAGE_COUNT + 1):
        draw_header_footer(c, page_num, PAGE_COUNT)
        y = height - 1.1 * inch

        if page_num == 1:
            y = draw_body_section1(c, y)
            y = draw_body_section2(c, y)
        elif page_num == 2:
            y = draw_table(c, y)
        elif page_num == 3:
            y = draw_product_y_section(c, y)
        elif page_num == 13:
            draw_garbled_page(c)
        elif page_num == 26:
            y = draw_ruled_table(c, y)
        elif page_num == 27:
            draw_two_column_page(c)
        else:
            c.setFont("Helvetica-Bold", 13)
            c.drawString(0.75 * inch, y, "Appendix Notes")
            y -= 22
            c.setFont("Helvetica", 10)
            filler = [
                f"Supplementary discussion continues on page {page_num}, covering",
                "operational metrics, compliance attestations, and audit trail",
                "references relevant to the fiscal year under review.",
            ]
            for line in filler:
                c.drawString(0.75 * inch, y, line)
                y -= 14

        c.showPage()
    c.save()
    print(f"Wrote {OUT_PATH}")

if __name__ == "__main__":
    main()
