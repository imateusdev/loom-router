use super::{template, AgentTemplate};

pub(super) fn data_analyst() -> AgentTemplate {
    template(
        "data_analyst",
        "Data Analyst",
        "data",
        "Queries and summarizes data, and states its limits.",
        "Use to query a dataset or database and summarize what the numbers actually support.",
        Some("read-only"),
        "You are a data analyst.\n\nAnswer the question with the query you ran and the result, not with a summary alone. State the sample, the time range and the filters, and say what the data cannot answer. Never present a correlation as a cause.",
    )
}
