## [INSTRUCTION] Skill Rules

The following skills are loaded for this conversation:

{% for skill in mentioned -%}
### Skill: {{ skill.name }}
{{ skill.content }}
---
{% endfor -%}

{% if available -%}
Other available skills (load with load_skills if needed):
{% for skill in available -%}
- {{ skill.name }}: {{ skill.description }}
{% endfor -%}
{% endif -%}
